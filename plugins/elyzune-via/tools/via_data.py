#!/usr/bin/env python3
"""Prepare the inputs of generate_tables.py from Mojang's server jars.

    via_data.py --data target/via-data --hyperion ../hyperion all

Steps (each can be run alone; all of them are idempotent):

  fetch      Mojang's version manifest, then the server jar of 1.21.11, 26.1.2,
             26.2 and 26.3 and the 1.21.11 server mappings (the last release
             that ships them). Every download is checked against the SHA-1 in
             Mojang's metadata. CFR 0.152 comes from Maven Central and is
             checked against Maven's published SHA-1.
  reports    The vanilla data generator of each jar
             (`java -DbundlerMainClass=net.minecraft.data.Main -jar server.jar
             --reports --server`): packet ids, registries, block states,
             datapack registries and tags.
  decompile  CFR over the classes the generator reads: packet and codec
             classes (the selection hyperion/nix/minecraft-data.nix uses),
             `RegistryDataLoader`, and every entity class. 26.x jars are
             unobfuscated; 1.21.11 is decompiled through its mappings and the
             output relocated to real package paths.
  extract    hyperion/nix/extract-protocol.py on the 26.x trees (wire layouts
             of every packet and data component). Its vocabulary check is
             relaxed for 26.3, which renamed a few FriendlyByteBuf helpers; an
             unknown helper makes a layout partial, never wrong.
  generate   tools/generate_tables.py into src/generated.

Nothing produced here is committed: the jars, reports and decompiled sources
stay under the data directory (in target/). Only the generated tables are.
Needs Java 25 (26.x jars) and network access to Mojang and Maven Central.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import urllib.request
import zipfile
from pathlib import Path

MANIFEST = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json"
RELEASES = ["1.21.11", "26.1.2", "26.2", "26.3"]
MAPPED = "1.21.11"
CFR_URL = "https://repo1.maven.org/maven2/org/benf/cfr/0.152/cfr-0.152.jar"


def log(msg: str) -> None:
    print(f"via_data: {msg}", file=sys.stderr)


def download(url: str, sha1: str | None, dest: Path) -> None:
    if dest.exists() and (sha1 is None or hashlib.sha1(dest.read_bytes()).hexdigest() == sha1):
        return
    data = urllib.request.urlopen(url, timeout=120).read()
    digest = hashlib.sha1(data).hexdigest()
    if sha1 is not None and digest != sha1:
        raise SystemExit(f"{url}: sha1 {digest} != {sha1}")
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(data)
    log(f"downloaded {dest.name} ({len(data)} bytes, sha1 {digest})")


def fetch(data: Path) -> None:
    dl = data / "dl"
    manifest = json.loads(urllib.request.urlopen(MANIFEST, timeout=60).read())
    versions = {v["id"]: v for v in manifest["versions"]}
    for release in RELEASES:
        entry = versions[release]
        meta_path = dl / f"{release}.json"
        download(entry["url"], entry["sha1"], meta_path)
        meta = json.loads(meta_path.read_text())
        server = meta["downloads"]["server"]
        download(server["url"], server["sha1"], dl / f"server-{release}.jar")
        if release == MAPPED:
            mappings = meta["downloads"]["server_mappings"]
            download(mappings["url"], mappings["sha1"], dl / f"server_mappings-{release}.txt")
    cfr_sha1 = urllib.request.urlopen(CFR_URL + ".sha1", timeout=60).read().decode().split()[0]
    download(CFR_URL, cfr_sha1, data / "tools" / "cfr-0.152.jar")


def reports(data: Path) -> None:
    for release in RELEASES:
        out = data / "gen" / release
        if (out / "generated" / "reports" / "packets.json").exists():
            continue
        out.mkdir(parents=True, exist_ok=True)
        jar = (data / "dl" / f"server-{release}.jar").resolve()
        subprocess.run(
            ["java", "-DbundlerMainClass=net.minecraft.data.Main", "-jar", str(jar),
             "--reports", "--server", "--output", "generated"],
            cwd=out, check=True, stdout=subprocess.DEVNULL,
        )
        with zipfile.ZipFile(jar) as z:
            (out / "version.json").write_bytes(z.read("version.json"))
        log(f"reports {release}")


def inner_classes(data: Path, release: str) -> Path:
    """Unpacks the bundled server jar's classes once."""
    work = data / "work" / release
    classes = work / "classes"
    if classes.is_dir():
        return classes
    with zipfile.ZipFile(data / "dl" / f"server-{release}.jar") as bundle:
        inner = next(n for n in bundle.namelist() if re.match(r"META-INF/versions/.*/server-.*\.jar$", n))
        work.mkdir(parents=True, exist_ok=True)
        bundle.extract(inner, work)
    with zipfile.ZipFile(work / inner) as z:
        z.extractall(classes)
    return classes


def cfr(data: Path, out: Path, files: list[Path], extra: list[str]) -> None:
    out.mkdir(parents=True, exist_ok=True)
    # CFR takes the class files on its command line; chunk to stay under the
    # argument limit.
    for i in range(0, len(files), 800):
        subprocess.run(
            ["java", "-Xmx4g", "-jar", str(data / "tools" / "cfr-0.152.jar"), "--outputdir", str(out),
             "--silent", "true", "--comments", "false", *extra, *map(str, files[i:i + 800])],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )


def outers_with_nested(classes: Path, rel_paths: set[str]) -> list[Path]:
    """Each selected class with its nested classes (CFR writes them into the
    outer class's file)."""
    outers = {p.split("$")[0].removesuffix(".class") for p in rel_paths}
    out = []
    for path in sorted(classes.rglob("*.class")):
        rel = path.relative_to(classes).as_posix().removesuffix(".class")
        if rel.split("$")[0] in outers:
            out.append(path)
    return out


def codec_selection(classes: Path, deobf: dict[str, str] | None = None) -> set[str]:
    """Network classes, classes naming StreamCodec, registry keys,
    RegistryDataLoader and the enums network classes reference."""
    name = (lambda rel: deobf.get(rel, rel)) if deobf else (lambda rel: rel)
    stream_codec = b"net/minecraft/network/codec/StreamCodec"
    if deobf:
        stream_codec = next(k for k, v in deobf.items() if v == "net/minecraft/network/codec/StreamCodec").encode()
    selected = set()
    network = []
    for path in classes.rglob("*.class"):
        rel = path.relative_to(classes).as_posix()
        real = name(rel.removesuffix(".class"))
        body = path.read_bytes()
        if real.startswith(("net/minecraft/network/", "net/minecraft/core/registries/")) or real == "net/minecraft/resources/RegistryDataLoader":
            selected.add(rel)
            network.append(body)
        elif stream_codec in body:
            selected.add(rel)
    for body in network:
        for ref in set(re.findall(rb"[A-Za-z0-9_/$]+", body)):
            candidate = classes / (ref.decode(errors="ignore") + ".class")
            if candidate.is_file() and b"java/lang/Enum" in candidate.read_bytes():
                selected.add(candidate.relative_to(classes).as_posix())
    return selected


def relocate(raw: Path, dest: Path) -> None:
    """Moves CFR output written under obfuscated file names to the real
    package path its `package` and class declaration name."""
    for path in raw.glob("*.java"):
        text = path.read_text()
        package = re.search(r"^package ([\w.]+);", text, re.M)
        cls = re.search(r"^(?:public |protected |private )?(?:abstract |final |sealed |non-sealed |static |strictfp )*"
                        r"(?:class|interface|enum|record|@interface) (\w+)", text, re.M)
        if not package or not cls:
            continue
        target = dest / package.group(1).replace(".", "/") / f"{cls.group(1)}.java"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)


def mappings(data: Path) -> dict[str, str]:
    deobf = {}
    for line in (data / "dl" / f"server_mappings-{MAPPED}.txt").read_text().splitlines():
        m = re.match(r"(\S+) -> (\S+):$", line)
        if m:
            deobf[m.group(2).replace(".", "/")] = m.group(1).replace(".", "/")
    return deobf


def decompile(data: Path) -> None:
    for release in RELEASES:
        classes = inner_classes(data, release)
        if release == MAPPED:
            deobf = mappings(data)
            extra = ["--obfuscationpath", str(data / "dl" / f"server_mappings-{MAPPED}.txt"),
                     "--extraclasspath", str(next((data / "work" / release).rglob("server-*.jar")))]
            target = data / "decomp" / f"{release}-named"
            if not target.is_dir():
                raw = data / "decomp" / f"{release}-raw"
                cfr(data, raw, outers_with_nested(classes, codec_selection(classes, deobf)), extra)
                relocate(raw, target)
                shutil.rmtree(raw)
            ent = data / "ent" / release
            if not ent.is_dir():
                raw = data / "ent" / f"{release}-raw"
                files = [p for p in classes.rglob("*.class")
                         if deobf.get(p.relative_to(classes).as_posix().removesuffix(".class"), "").startswith("net/minecraft/world/entity/")]
                cfr(data, raw, files, extra)
                relocate(raw, ent)
                shutil.rmtree(raw)
        else:
            target = data / "decomp" / release
            if not target.is_dir():
                cfr(data, target, outers_with_nested(classes, codec_selection(classes)), [])
            ent = data / "ent" / release
            if not ent.is_dir():
                cfr(data, ent, sorted((classes / "net/minecraft/world/entity").rglob("*.class")), [])
        log(f"decompiled {release}")


def extract(data: Path, hyperion: Path) -> None:
    source = (hyperion / "nix" / "extract-protocol.py").read_text()
    lenient = data / "tools" / "extract-protocol-lenient.py"
    lenient.parent.mkdir(parents=True, exist_ok=True)
    strict = "    missing = check_vocabulary(index)\n    if missing:\n"
    if strict not in source:
        raise SystemExit("extract-protocol.py changed: the vocabulary check to relax was not found")
    lenient.write_text(source.replace(strict, "    missing = check_vocabulary(index)\n    if missing and False:\n", 1))
    (data / "proto").mkdir(parents=True, exist_ok=True)
    for release in RELEASES:
        if release == MAPPED:
            continue
        out = data / "proto" / f"{release}.json"
        if out.exists():
            continue
        result = subprocess.run(
            [sys.executable, "-I", str(lenient), "--generated", str(data / "gen" / release / "generated"),
             "--decompiled", str(data / "decomp" / release), "--version-json", str(data / "gen" / release / "version.json"),
             "--out", str(out)],
            capture_output=True, text=True,
        )
        # The extractor exits non-zero for an enum it cannot scan (26.1 has
        # no TeamColor); the protocol description is still written.
        if not out.exists():
            raise SystemExit(f"extractor failed for {release}:\n{result.stderr[-2000:]}")
        log(f"extracted {release}")


def generate(data: Path) -> None:
    here = Path(__file__).resolve().parent
    subprocess.run([sys.executable, "-I", str(here / "generate_tables.py"), "--data", str(data),
                    "--out", str(here.parent / "src" / "generated")], check=True)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--data", required=True, type=Path)
    ap.add_argument("--hyperion", type=Path, help="hyperion checkout (for nix/extract-protocol.py)")
    ap.add_argument("steps", nargs="+", choices=["fetch", "reports", "decompile", "extract", "generate", "all"])
    args = ap.parse_args()
    steps = ["fetch", "reports", "decompile", "extract", "generate"] if "all" in args.steps else args.steps
    for s in steps:
        if s == "extract":
            if args.hyperion is None:
                raise SystemExit("extract needs --hyperion")
            extract(args.data, args.hyperion)
        else:
            globals()[s](args.data)
    return 0


if __name__ == "__main__":
    sys.exit(main())
