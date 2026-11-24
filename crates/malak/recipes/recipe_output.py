"""Shared human output for shell and Python recipes."""

import argparse
import os
import re
import shutil
import sys
import unicodedata


def display(value):
    text = []
    for character in str(value):
        code = ord(character)
        category = unicodedata.category(character)
        if category == "Cc" or code in (0x2028, 0x2029):
            visible = {"\n": r"\n", "\r": r"\r", "\t": r"\t"}
            text.append(visible.get(character, r"\u{" + f"{code:x}" + "}"))
        elif category != "Cf" and code not in (0x034f, 0x3164, 0xffa0) and not any(
            low <= code <= high
            for low, high in ((0x115f, 0x1160), (0x17b4, 0x17b5), (0x180b, 0x180f),
                              (0xfe00, 0xfe0f), (0xfff0, 0xfff8), (0xe0000, 0xe0fff))
        ):
            text.append(character)
    return "".join(text)


def width(value):
    return sum(
        0 if unicodedata.combining(character) else
        2 if unicodedata.east_asian_width(character) in ("W", "F") else 1
        for character in value
    )


def interactive(stream):
    return stream.isatty() and os.environ.get("TERM") != "dumb"


def columns(stream):
    try:
        return os.get_terminal_size(stream.fileno()).columns or 80
    except OSError:
        return 80


def styled(text, sgr, stream):
    if interactive(stream) and "NO_COLOR" not in os.environ:
        return f"\x1b[{sgr}m{text}\x1b[0m"
    return text


def wrapped(text, limit):
    """Break at spaces and leave long addresses and IDs intact."""
    line = ""
    for word in re.findall(r"[^ ]+ *| +", text):
        if line and width(line + word.rstrip(" ")) > limit:
            yield line.rstrip(" ")
            line = ""
        line += word
    yield line.rstrip(" ")


def paragraph(value, *, stream, indent="  "):
    limit = columns(stream) - width(indent)
    for line in str(value).split("\n"):
        for part in wrapped(display(line), limit):
            print(indent + part, file=stream)


def section(title, *, stream, footer=False):
    print(file=stream)
    rule = ("═" if footer else "─") * min(columns(stream), 80)
    print(styled(rule, 2, stream), file=stream)
    print(styled(title, 2 if footer else 1, stream), file=stream)


def next_step(step, *, stream, command=False):
    section("Next", stream=stream, footer=True)
    step = display(step)
    lines = (step,) if command else wrapped(step, columns(stream) - 2)
    for line in lines:
        print(styled("  " + line, 2, stream), file=stream)


def fields_output(fields, *, stream, compact=False, indent="  "):
    span = max((width(label) for label, _ in fields), default=0)
    for label, value in fields:
        label, value = display(label), display(value)
        prefix = indent + label + " " * (span - width(label) + 2)
        if not compact:
            print(prefix + value, file=stream)
            continue
        limit = columns(stream) - width(prefix)
        if width(value) > limit and (
            limit < 16 or any(width(word) > limit for word in value.split())
        ):
            print(indent + label, file=stream)
            paragraph(value, stream=stream, indent=indent + "  ")
        else:
            for index, line in enumerate(wrapped(value, limit)):
                print((prefix if index == 0 else " " * width(prefix)) + line, file=stream)


def details(title, fields=(), *, stream=None, group=None, next_label="Next"):
    if stream is None:
        stream = sys.stdout
    if not interactive(stream):
        print(display(title), file=stream)
        fields_output(fields, stream=stream)
        return
    for line in wrapped(display(title), columns(stream)):
        print(styled(line, 1, stream), file=stream)
    ordinary = []
    step = None
    for label, value in fields:
        if label == next_label:
            step = value
        else:
            ordinary.append((label, value))
    if ordinary and group:
        section(group, stream=stream)
    fields_output(ordinary, stream=stream, compact=True)
    if step is not None:
        next_step(step, stream=stream)


def error(operation, reason, next_step, *, stream=None):
    reason = " ".join(line.strip(" ") for line in str(reason).split("\n"))
    details(
        f"Error: Could not {operation}.",
        (("Reason", reason), ("Next", next_step)),
        stream=sys.stderr if stream is None else stream,
        group="Details",
    )


def table(title, headers, rows, empty):
    if not rows:
        details(empty)
        return
    details(title)
    rows = [tuple(map(display, row)) for row in rows]
    cells_by_column = tuple(zip(headers, *rows))
    widths = tuple(max(map(width, column)) for column in cells_by_column)
    terminal = interactive(sys.stdout)
    if terminal:
        limit = columns(sys.stdout)
        name_width = max(width(headers[0]), min(widths[0], 12))
        visible = len(headers)
        while visible > 1 and name_width + sum(widths[1:visible]) + 2 * visible > limit:
            visible -= 1
        available = max(name_width, limit - sum(widths[1:visible]) - 2 * visible)
        widths = (min(widths[0], available), *widths[1:])
        rows = [(short_name(row[0], widths[0]), *row[1:]) for row in rows]
    elif sum(widths) + 2 * len(headers) > (
        shutil.get_terminal_size((80, 24)).columns if sys.stdout.isatty() else 80
    ):
        for row in rows:
            for label, value in zip(headers, row):
                print(f"  {label}  {value}")
            print()
        return
    else:
        visible = len(headers)
    numeric = tuple(
        (index > 0 or not terminal)
        and any(cell.isdecimal() for cell in column[1:])
        and all(cell.isdecimal() or cell == "Unavailable" for cell in column[1:])
        for index, column in enumerate(cells_by_column)
    )
    for index, row in enumerate((headers, *rows)):
        cells = []
        for value, span, right in zip(row[:visible], widths, numeric):
            pad = " " * (span - width(value))
            cells.append(pad + value if right else value + pad)
        print(("  " + "  ".join(cells)).rstrip())
        if index and visible < len(headers):
            fields_output(tuple(zip(headers[visible:], row[visible:])),
                          stream=sys.stdout, compact=True, indent="    ")


def short_name(value, limit):
    if width(value) <= limit:
        return value
    end = 0
    while end < len(value) and width(value[:end + 1]) < limit:
        end += 1
    return value[:end] + "…"


def receipt(title, value, fields=()):
    durability = {
        "memory": "In memory",
        "restart": "Survives a board restart",
        "synced": "Synced to storage",
    }
    duplicate = (("Delivery", "Already recorded"),) if value["duplicate"] else ()
    details(title, (
        *fields,
        ("Message ID", value["id"]),
        ("Creation", value["creation"]),
        ("Durability", durability.get(value["durability"], value["durability"])),
        *duplicate,
    ), group="Receipt")


def usage(lines, *, stream=None, reason=None, description=None):
    stream = sys.stdout if stream is None else stream
    about = (description or os.environ.get("MALAK_RECIPE_DESCRIPTION")) if reason is None else None
    if not interactive(stream):
        if about:
            print(about, "", sep="\n", file=stream)
        print("\n".join(lines), file=stream)
        if reason is None:
            example(stream)
        return
    details("Malak recipe help" if reason is None else "Error: Could not run the recipe.", stream=stream)
    if about:
        paragraph(about, stream=stream)
    if reason is not None:
        section("Reason", stream=stream)
        paragraph(reason, stream=stream)
    for line in lines:
        if line.lower().startswith("usage: "):
            section("Usage", stream=stream)
            line = line[7:]
        elif line == "options:":
            section("Options", stream=stream)
            continue
        if line:
            paragraph(line.strip(), stream=stream)
    if reason is not None:
        next_step("Use --help with this command for usage.", stream=stream)
    else:
        example(stream)


def example(stream):
    snippet = os.environ.get("MALAK_RECIPE_EXAMPLE")
    if snippet:
        if interactive(stream):
            section("Example", stream=stream)
        else:
            print("\nExample:", file=stream)
        print(snippet, file=stream)


class ArgumentParser(argparse.ArgumentParser):
    def print_help(self, file=None):
        stream = sys.stdout if file is None else file
        help_text = self.format_help()
        if interactive(stream):
            help_text = re.sub(r"\x1b\[[0-9;]*m", "", help_text)
        parts = help_text.split("\n\n", 2)
        description = None
        if self.description and len(parts) == 3:
            syntax, description, options = parts
            help_text = f"{syntax}\n\n{options}"
        usage(help_text.rstrip().splitlines(), stream=stream, description=description)

    def error(self, message):
        if not interactive(sys.stderr):
            super().error(message)
        help_text = re.sub(r"\x1b\[[0-9;]*m", "", self.format_usage())
        usage(help_text.splitlines(), stream=sys.stderr, reason=message)
        self.exit(2)


if __name__ == "__main__":
    mode, title, *args = sys.argv[1:]
    if mode == "error":
        error(title, *args)
    elif mode == "usage":
        reason = None if title == "help" else "Missing or invalid recipe arguments."
        usage(args, reason=reason)
    elif mode == "notice":
        error(title, *args, stream=sys.stdout)
    else:
        stream = sys.stderr if mode == "diagnostic" else sys.stdout
        details(title, tuple(zip(args[::2], args[1::2])), stream=stream)
