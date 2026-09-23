#!/usr/bin/env python3
"""Flags calls to functions that nothing in the file declares or imports.

Deleting a helper during a refactor and leaving its callers behind produces a
ReferenceError only when that branch runs, which in a UI can be much later --
once via a screenshot, once via a user. This catches it at build time.

Deliberately conservative: it only looks at call sites, treats any name bound
anywhere in the file as declared, and carries an allowlist of globals. It would
rather miss something than cry wolf.
"""
import re
import sys
from pathlib import Path

GLOBALS = {
    # Language and runtime
    "Array", "Boolean", "Date", "Error", "JSON", "Map", "Math", "Number",
    "Object", "Promise", "RegExp", "Set", "String", "Symbol", "WeakMap",
    "parseFloat", "parseInt", "isNaN", "isFinite", "decodeURIComponent",
    "encodeURIComponent", "structuredClone", "queueMicrotask",
    # Browser
    "alert", "cancelAnimationFrame", "clearInterval", "clearTimeout", "fetch",
    "getComputedStyle", "requestAnimationFrame", "setInterval", "setTimeout",
    # Keywords that a naive regex reads as calls
    "if", "for", "while", "switch", "catch", "return", "typeof", "function",
    "await", "new", "delete", "void", "in", "of", "do", "else", "case",
    "async", "yield", "try", "throw", "super", "this",
}

CALL = re.compile(r"(?<![.\w$])([A-Za-z_$][\w$]*)\s*\(")


def strip_noise(src: str) -> str:
    """Removes comments and string text, keeping code.

    Prose is full of things that look like calls -- "van Wijk (2000)", "reach
    the backend (" -- so scanning raw source reports them as missing
    functions. Template literals keep the contents of their ${...} holes,
    because those are real code.
    """
    out: list[str] = []
    i, n = 0, len(src)
    # A regex literal can contain quotes -- /[&<>"\']/g is in this very
    # codebase -- and reading one as the start of a string desynchronises
    # everything after it. Telling a regex from a division needs the previous
    # token: after a value, "/" divides; after an operator or an opening
    # bracket, it opens a pattern.
    def opens_regex() -> bool:
        for ch in reversed(out):
            if ch.isspace():
                continue
            return ch in "(,=:[!&|?{};+-*%~^<>"
        return True

    while i < n:
        c = src[i]
        pair = src[i:i + 2]
        if pair == "//":
            i = src.find("\n", i)
            if i == -1:
                break
        elif pair == "/*":
            end = src.find("*/", i + 2)
            i = n if end == -1 else end + 2
        elif c == "/" and opens_regex():
            i += 1
            in_class = False
            while i < n:
                if src[i] == "\\":
                    i += 2
                    continue
                if src[i] == "[":
                    in_class = True
                elif src[i] == "]":
                    in_class = False
                elif src[i] == "/" and not in_class:
                    break
                elif src[i] == "\n":
                    break
                i += 1
            i += 1
            out.append("RE")
        elif c in "'\"":
            i += 1
            while i < n and src[i] != c:
                i += 2 if src[i] == "\\" else 1
            i += 1
            out.append('""')
        elif c == "`":
            i += 1
            depth = 0
            while i < n:
                if src[i] == "\\":
                    i += 2
                    continue
                if src[i:i + 2] == "${":
                    depth += 1
                    i += 2
                    start = i
                    # Keep the expression inside the hole; it is real code.
                    while i < n and depth:
                        if src[i] == "{":
                            depth += 1
                        elif src[i] == "}":
                            depth -= 1
                            if not depth:
                                break
                        i += 1
                    out.append(" " + src[start:i] + " ")
                    i += 1
                    continue
                if src[i] == "`":
                    break
                i += 1
            i += 1
            out.append('""')
        else:
            out.append(c)
            i += 1
    return "".join(out)

def declared_names(src: str) -> set[str]:
    names: set[str] = set()
    # Declarations
    names |= set(re.findall(r"\b(?:function|class)\s+([A-Za-z_$][\w$]*)", src))
    names |= set(re.findall(r"\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)", src))
    # Imports, including destructured ones
    for group in re.findall(r"import\s*\{([^}]*)\}", src):
        names |= {n.strip().split(" as ")[-1].strip() for n in group.split(",") if n.strip()}
    names |= set(re.findall(r"import\s+([A-Za-z_$][\w$]*)\s+from", src))
    # Anything bound as a parameter or by destructuring counts as declared.
    for group in re.findall(r"\(([^()]*)\)\s*=>", src):
        names |= set(re.findall(r"[A-Za-z_$][\w$]*", group))
    for group in re.findall(r"function\s*[A-Za-z_$\w]*\s*\(([^()]*)\)", src):
        names |= set(re.findall(r"[A-Za-z_$][\w$]*", group))
    for group in re.findall(r"(?:const|let|var)\s*\{([^}]*)\}", src):
        names |= set(re.findall(r"[A-Za-z_$][\w$]*", group))
    names |= set(re.findall(r"catch\s*\(\s*([A-Za-z_$][\w$]*)", src))
    names |= set(re.findall(r"for\s*\(\s*(?:const|let|var)\s+([A-Za-z_$][\w$]*)", src))
    # Single-parameter arrows without parentheses: `x => ...`
    names |= set(re.findall(r"(?:^|[(,=\s])([A-Za-z_$][\w$]*)\s*=>", src, re.M))
    return names

def main() -> int:
    failed = False
    for path in sorted(Path(__file__).parent.glob("*.js")):
        src = strip_noise(path.read_text(encoding="utf-8"))
        known = declared_names(src) | GLOBALS
        missing = sorted({n for n in CALL.findall(src) if n not in known})
        if missing:
            failed = True
            print(f"{path.name}: called but never declared: {', '.join(missing)}")
    if not failed:
        print("ui: every called function is declared")
    return 1 if failed else 0

if __name__ == "__main__":
    sys.exit(main())
