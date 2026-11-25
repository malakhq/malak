#!/usr/bin/env python3
"""List the public API of the `malak` crate and compare it with the snapshot.

The listing comes from rustdoc's JSON output. That format is unstable, so the
tool runs only on the toolchain it was written for (FORMAT_VERSION below, the
one CI pins in .github/workflows/ci.yml) and asks stable rustc for it with
RUSTC_BOOTSTRAP. It needs no network, no nightly toolchain and no extra tool.
A toolchain bump regenerates the snapshot in the same change:

    scripts/public_api.py            compare with docs/public-api.txt
    scripts/public_api.py --bless    rewrite docs/public-api.txt

A public API change has to be deliberate: the snapshot changes in the same
commit, and CHANGELOG.md says why.
"""

import difflib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SNAPSHOT = ROOT / "docs/public-api.txt"
TARGET = ROOT / "target/public-api"
CRATE = "malak"
# The rustdoc JSON format version of the pinned toolchain.
FORMAT_VERSION = 61


class Crates:
    """The rustdoc JSON of every workspace crate that the listing reaches."""

    def __init__(self, hidden=False):
        self.loaded = {}
        self.hidden = hidden

    def load(self, name):
        if name not in self.loaded:
            package = name.replace("_", "-")
            env = {**os.environ, "RUSTC_BOOTSTRAP": "1"}
            # rustdoc leaves doc(hidden) items out of its JSON unless asked.
            target = TARGET.with_name(TARGET.name + ("-hidden" if self.hidden else ""))
            subprocess.run(
                ["cargo", "rustdoc", "--locked", "-q", "-p", package, "--lib",
                 "--target-dir", str(target), "--", "-Zunstable-options",
                 "--output-format", "json", *(["--document-hidden-items"] if self.hidden else [])],
                cwd=ROOT, env=env, check=True,
            )
            data = json.loads((target / "doc" / f"{name}.json").read_text())
            if data["format_version"] != FORMAT_VERSION:
                sys.exit(
                    f"rustdoc JSON format {data['format_version']}, expected "
                    f"{FORMAT_VERSION}: regenerate the snapshot with --bless on the "
                    "pinned toolchain and update FORMAT_VERSION."
                )
            self.loaded[name] = Crate(self, name, data)
        return self.loaded[name]


class Crate:
    def __init__(self, crates, name, data):
        self.crates, self.name, self.data = crates, name, data
        self.by_path = {
            tuple(entry["path"]): int(identifier)
            for identifier, entry in data["paths"].items() if entry["crate_id"] == 0
        }

    def item(self, identifier):
        return self.data["index"][str(identifier)]

    def resolve(self, identifier):
        """Find the crate and item a use or type path refers to, or None."""
        if str(identifier) in self.data["index"]:
            return self, self.item(identifier)
        entry = self.data["paths"].get(str(identifier))
        if entry is None or entry["crate_id"] == 0:
            return None
        outside = self.data["external_crates"][str(entry["crate_id"])]["name"]
        if not outside.startswith("malak_"):
            return None
        other = self.crates.load(outside)
        found = other.by_path.get(tuple(entry["path"]))
        return (other, other.item(found)) if found is not None else None


def hidden(item):
    return item["visibility"] != "public" or any(
        "doc(hidden)" in (attr if isinstance(attr, str) else json.dumps(attr))
        for attr in item["attrs"]
    )


def render_type(kind):
    if kind is None:
        return "_"
    (tag, body), = kind.items() if isinstance(kind, dict) else ((kind, None),)
    if tag == "primitive" or tag == "generic":
        return body
    if tag == "resolved_path":
        return body["path"] + render_args(body.get("args"))
    if tag == "borrowed_ref":
        lifetime = f"{body['lifetime']} " if body.get("lifetime") else ""
        mutable = "mut " if body["is_mutable"] else ""
        return f"&{lifetime}{mutable}{render_type(body['type'])}"
    if tag == "tuple":
        return "(" + ", ".join(map(render_type, body)) + ")"
    if tag == "slice":
        return f"[{render_type(body)}]"
    if tag == "array":
        return f"[{render_type(body['type'])}; {body['len']}]"
    if tag == "raw_pointer":
        return f"*{'mut' if body['is_mutable'] else 'const'} {render_type(body['type'])}"
    if tag == "impl_trait":
        return "impl " + render_bounds(body)
    if tag == "dyn_trait":
        return "dyn " + " + ".join(render_trait(bound["trait"]) for bound in body["traits"])
    if tag == "qualified_path":
        owner = render_type(body["self_type"])
        return f"{owner}::{body['name']}" if owner == "Self" else f"<{owner}>::{body['name']}"
    if tag == "function_pointer":
        arguments = ", ".join(render_type(kind) for _, kind in body["sig"]["inputs"])
        output = body["sig"]["output"]
        return f"fn({arguments})" + (f" -> {render_type(output)}" if output else "")
    return f"<{tag}>"


def render_trait(trait):
    return trait["path"] + render_args(trait.get("args"))


def render_args(args):
    if not args or "angle_bracketed" not in args:
        return ""
    parts = []
    for argument in args["angle_bracketed"]["args"]:
        (tag, body), = argument.items()
        parts.append(render_type(body) if tag == "type" else str(body))
    parts += [f"{c['name']} = {render_type(c['binding']['equality']['type'])}"
              if "equality" in c["binding"] else c["name"]
              for c in args["angle_bracketed"]["constraints"]]
    return f"<{', '.join(parts)}>" if parts else ""


def render_bounds(bounds):
    names = []
    for bound in bounds:
        if "trait_bound" in bound:
            names.append(render_trait(bound["trait_bound"]["trait"]))
        elif "outlives" in bound:
            names.append(bound["outlives"])
    return " + ".join(names)


def render_generics(generics, with_where=True):
    params = []
    for parameter in generics["params"]:
        kind = parameter["kind"]
        if "lifetime" in kind:
            outlives = kind["lifetime"]["outlives"]
            params.append(parameter["name"] + (": " + " + ".join(outlives) if outlives else ""))
        elif "type" in kind:
            if kind["type"].get("is_synthetic"):
                continue
            bounds = render_bounds(kind["type"]["bounds"])
            params.append(parameter["name"] + (f": {bounds}" if bounds else ""))
        else:
            params.append(f"const {parameter['name']}: {render_type(kind['const']['type'])}")
    text = f"<{', '.join(params)}>" if params else ""
    clauses = []
    for predicate in generics["where_predicates"] if with_where else []:
        if "bound_predicate" in predicate:
            bound = predicate["bound_predicate"]
            clauses.append(f"{render_type(bound['type'])}: {render_bounds(bound['bounds'])}")
    return text + (" where " + ", ".join(clauses) if clauses else "")


def render_function(name, function):
    signature = function["sig"]
    receivers = {"Self": "self", "&Self": "&self", "&mut Self": "&mut self"}
    arguments = []
    for argument, kind in signature["inputs"]:
        shown = render_type(kind)
        if argument == "self":
            arguments.append(receivers.get(shown, f"self: {shown}"))
        else:
            arguments.append(f"{argument}: {shown}")
    header = function["header"]
    prefix = "".join(f"{word} " for word, on in (
        ("const", header["is_const"]), ("async", header["is_async"]),
        ("unsafe", header["is_unsafe"])) if on)
    output = f" -> {render_type(signature['output'])}" if signature["output"] else ""
    return (f"{prefix}fn {name}{render_generics(function['generics'], False)}"
            f"({', '.join(arguments)}){output}"
            + render_generics({"params": [], "where_predicates": function["generics"]["where_predicates"]}))


def lines_for(crate, item, path):
    """Yield the listing lines for one public item at its public path."""
    inner = item["inner"]
    (kind, body), = inner.items()
    if kind == "function":
        yield f"pub {render_function(path, body)}"
    elif kind == "struct":
        yield from struct_lines(crate, body, path)
        yield from impl_lines(crate, body["impls"], path)
    elif kind == "enum":
        yield f"pub enum {path}{render_generics(body['generics'])}"
        for variant_id in body["variants"]:
            variant = crate.item(variant_id)
            yield f"pub variant {path}::{variant['name']}{variant_shape(crate, variant)}"
        yield from impl_lines(crate, body["impls"], path)
    elif kind == "trait":
        supers = render_bounds(body["bounds"])
        yield (f"pub trait {path}{render_generics(body['generics'])}"
               + (f": {supers}" if supers else ""))
        for child_id in body["items"]:
            child = crate.item(child_id)
            yield from trait_item_lines(child, path)
    elif kind == "type_alias":
        yield f"pub type {path}{render_generics(body['generics'])} = {render_type(body['type'])}"
    elif kind in ("constant", "static"):
        yield f"pub {kind} {path}: {render_type(body['type'])}"
    elif kind == "module":
        yield f"pub mod {path}"
    else:
        yield f"pub {kind} {path}"


def trait_item_lines(child, path):
    (kind, body), = child["inner"].items()
    if kind == "function":
        yield f"pub trait {render_function(f'{path}::{child['name']}', body)}"
    elif kind == "assoc_type":
        yield f"pub trait type {path}::{child['name']}"
    elif kind == "assoc_const":
        yield f"pub trait const {path}::{child['name']}: {render_type(body['type'])}"


def struct_lines(crate, body, path):
    generics = render_generics(body["generics"])
    shape = body["kind"]
    if shape == "unit":
        yield f"pub struct {path}{generics}"
    elif "tuple" in shape:
        shown = [render_type(crate.item(f)["inner"]["struct_field"]) for f in shape["tuple"]
                 if f is not None and not hidden(crate.item(f))]
        private = any(f is None or hidden(crate.item(f)) for f in shape["tuple"])
        shown += [".."] if private else []
        yield f"pub struct {path}{generics}({', '.join(shown)})"
    else:
        yield f"pub struct {path}{generics}"
        for field_id in shape["plain"]["fields"]:
            field = crate.item(field_id)
            if not hidden(field):
                yield f"pub field {path}.{field['name']}: {render_type(field['inner']['struct_field'])}"


def variant_shape(crate, variant):
    shape = variant["inner"]["variant"]["kind"]
    if shape == "plain":
        return ""
    if "tuple" in shape:
        return "(" + ", ".join(render_type(crate.item(f)["inner"]["struct_field"])
                               for f in shape["tuple"] if f is not None) + ")"
    fields = (f"{crate.item(f)['name']}: {render_type(crate.item(f)['inner']['struct_field'])}"
              for f in shape["struct"]["fields"])
    return " { " + ", ".join(fields) + " }"


def impl_lines(crate, impl_ids, path):
    for impl_id in impl_ids:
        impl = crate.item(impl_id)["inner"]["impl"]
        if impl["is_synthetic"] or impl["blanket_impl"] is not None:
            continue
        generics = render_generics(impl["generics"])
        if impl["trait"] is not None:
            yield f"impl{generics} {render_trait(impl['trait'])} for {path}"
            continue
        for child_id in impl["items"]:
            child = crate.item(child_id)
            if hidden(child):
                continue
            (kind, body), = child["inner"].items()
            if kind == "function":
                yield f"pub {render_function(f'{path}::{child['name']}', body)}"
            elif kind == "assoc_const":
                yield f"pub const {path}::{child['name']}: {render_type(body['type'])}"


def walk(crate, module, prefix, out):
    for child_id in module["inner"]["module"]["items"]:
        child = crate.item(child_id)
        if hidden(child):
            continue
        (kind, body), = child["inner"].items()
        if kind == "use":
            walk_use(crate, body, prefix, out)
        elif kind == "module":
            path = f"{prefix}::{child['name']}"
            out.append(f"pub mod {path}")
            walk(crate, child, path, out)
        else:
            out.extend(lines_for(crate, child, f"{prefix}::{child['name']}"))


def walk_use(crate, use, prefix, out):
    found = crate.resolve(use["id"]) if use["id"] is not None else None
    if found is None:
        out.append(f"pub use {prefix}::{use['name']} = {use['source']}")
        return
    target_crate, target = found
    if hidden(target):
        return
    if "module" in target["inner"]:
        name = prefix if use["is_glob"] else f"{prefix}::{use['name']}"
        if not use["is_glob"]:
            out.append(f"pub mod {name}")
        walk(target_crate, target, name, out)
    else:
        out.extend(lines_for(target_crate, target, f"{prefix}::{use['name']}"))


def hidden_names():
    """The names of public items the listing leaves out because they are doc(hidden)."""
    crates = Crates(hidden=True)
    names = set()
    for name in ("malak_core", "malak_layers"):
        for item in crates.load(name).data["index"].values():
            if item["crate_id"] == 0 and item.get("name") and item["visibility"] == "public" and any(
                "doc(hidden)" in (attr if isinstance(attr, str) else json.dumps(attr))
                for attr in item["attrs"]
            ):
                names.add(item["name"])
    return names


def public_names(listing_text):
    """Every identifier the listing mentions, for telling hidden-only names apart."""
    return set(re.findall(r"\w+", listing_text))


def listing():
    crate = Crates().load(CRATE)
    out = []
    walk(crate, crate.item(crate.data["root"]), CRATE, out)
    return "\n".join(sorted(set(out))) + "\n"


def main(arguments):
    current = listing()
    if arguments == ["--bless"]:
        SNAPSHOT.write_text(current)
        print(f"wrote {SNAPSHOT.relative_to(ROOT)}")
        return 0
    expected = SNAPSHOT.read_text() if SNAPSHOT.exists() else ""
    if current == expected:
        print("public API matches docs/public-api.txt")
        return 0
    sys.stdout.writelines(difflib.unified_diff(
        expected.splitlines(True), current.splitlines(True),
        "docs/public-api.txt", "current public API",
    ))
    print(
        "\nThe public API of the malak crate changed. If that is deliberate, run\n"
        "scripts/public_api.py --bless, commit docs/public-api.txt and say why in\n"
        "CHANGELOG.md. Otherwise hide the item with #[doc(hidden)] or pub(crate).",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
