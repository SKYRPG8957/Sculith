"""Build Sculith Companion using the Java 26.3 client and Fabric compile classpath."""
import argparse
from pathlib import Path
import subprocess
import tempfile
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo


def build(javac, classpath, destination):
    root = Path(__file__).resolve().parent
    sources = sorted((root / "src/main/java").rglob("*.java"))
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as temp:
        classes = Path(temp) / "classes"
        classes.mkdir()
        subprocess.run([javac, "--release", "25", "-encoding", "UTF-8", "-proc:none", "-cp", classpath,
                        "-d", classes, *sources], check=True)
        entries = {"META-INF/LICENSE": (root.parent / "LICENSE").read_bytes()}
        for base in (classes, root / "src/main/resources"):
            entries.update({f.relative_to(base).as_posix(): f.read_bytes() for f in base.rglob("*") if f.is_file()})
        archive = Path(temp) / "companion.jar"
        with ZipFile(archive, "w", compression=ZIP_DEFLATED) as jar:
            for name, data in sorted(entries.items()):
                info = ZipInfo(name, (2026, 1, 1, 0, 0, 0))
                info.compress_type = ZIP_DEFLATED
                jar.writestr(info, data)
        destination.write_bytes(archive.read_bytes())


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--classpath", required=True, help="Mojang client, its libraries, Fabric Loader, Mixin, Fabric API Base and Screen API")
    parser.add_argument("--javac", default="javac")
    parser.add_argument("--output", type=Path, default=Path("sculith-companion-26.3.jar"))
    options = parser.parse_args()
    build(options.javac, options.classpath, options.output)
