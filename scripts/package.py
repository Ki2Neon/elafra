#!/usr/bin/env python3
"""Stage the pinned PDFium library or build a native Elafra package."""

import argparse
import hashlib
import os
from pathlib import Path, PurePosixPath
import platform
import plistlib
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import urllib.request
import zipfile


ROOT = Path(__file__).resolve().parents[1]
MAC_ICON = ROOT / "assets" / "elafra.icns"
PDFIUM_VERSION = "8125"
PDFIUM_RELEASE = (
    "https://github.com/bblanchon/pdfium-binaries/releases/download"
    f"/chromium/{PDFIUM_VERSION}"
)
PDFIUM_BUILDS = {
    "win-x64": (
        "bin/pdfium.dll",
        "9a964261760cff52570aabf038ab346d3a5034dd8b30d0d5fe8861235d356b8b",
    ),
    "mac-arm64": (
        "lib/libpdfium.dylib",
        "4939bd935ac83ddc09a3a4f038bc43a19cea1660316a3667f26cb07a070e139d",
    ),
    "mac-x64": (
        "lib/libpdfium.dylib",
        "8a5193535b80c7215d8932f6647ae5097465b6dba2811602c21e120c2af112b0",
    ),
    "linux-x64": (
        "lib/libpdfium.so",
        "e1c3ce6e270cb5e0c20816b379ad9f2c1309972697c505f508de92bf5e7d6659",
    ),
}


def native_target():
    # Use the Rust toolchain architecture, including when Python runs under Rosetta.
    version_info = subprocess.check_output(["rustc", "-vV"], text=True, cwd=ROOT)
    host = next(line.removeprefix("host: ") for line in version_info.splitlines()
                if line.startswith("host: "))
    targets = {
        ("Windows", "x86_64-pc-windows-msvc"): "win-x64",
        ("Darwin", "aarch64-apple-darwin"): "mac-arm64",
        ("Darwin", "x86_64-apple-darwin"): "mac-x64",
        ("Linux", "x86_64-unknown-linux-gnu"): "linux-x64",
    }
    try:
        return targets[platform.system(), host], host
    except KeyError:
        raise ValueError(f"Unsupported native Rust toolchain: {host}") from None


def matches_checksum(file, expected_checksum):
    with file.open("rb") as file_handle:
        return hashlib.file_digest(file_handle, "sha256").hexdigest() == expected_checksum


def fetch_pdfium(target, cache):
    archive_name = f"pdfium-{target}.tgz"
    cached_archive = cache / archive_name
    _, checksum = PDFIUM_BUILDS[target]
    if cached_archive.is_file() and matches_checksum(cached_archive, checksum):
        return cached_archive

    cache.mkdir(parents=True, exist_ok=True)
    print(f"Downloading PDFium {PDFIUM_VERSION} for {target}", flush=True)
    with tempfile.TemporaryDirectory(dir=cache) as temporary:
        download = Path(temporary) / archive_name
        with urllib.request.urlopen(f"{PDFIUM_RELEASE}/{archive_name}", timeout=60) as response:
            with download.open("wb") as file_handle:
                shutil.copyfileobj(response, file_handle)
        if not matches_checksum(download, checksum):
            raise ValueError(f"SHA-256 mismatch: {archive_name}")
        download.replace(cached_archive)
    return cached_archive


def copy_member(archive, member, destination):
    if not member.isfile():
        raise ValueError(f"Expected a regular PDFium archive member: {member.name}")
    with archive.extractfile(member) as contents:
        payload = contents.read()
    if destination.is_file() and destination.read_bytes() == payload:
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=destination.parent) as temporary:
        staged = Path(temporary) / destination.name
        staged.write_bytes(payload)
        staged.replace(destination)


def stage_pdfium(archive_path, target, library_dir, license_dir=None):
    library_member, _ = PDFIUM_BUILDS[target]
    library = library_dir / PurePosixPath(library_member).name
    with tarfile.open(archive_path, "r:gz") as archive:
        copy_member(archive, archive.getmember(library_member), library)
        if license_dir is not None:
            copy_member(archive, archive.getmember("LICENSE"), license_dir / "LICENSE")
            copy_member(archive, archive.getmember("VERSION"), license_dir / "VERSION")
            for member in archive.getmembers():
                path = PurePosixPath(member.name)
                if path.parent == PurePosixPath("licenses") and member.isfile():
                    copy_member(archive, member, license_dir / path.name)
    return library


def prepare(archive, target, profile_dir):
    stage_pdfium(archive, target, profile_dir)
    # Cargo's executable for unit tests lives under deps/ and uses the same loader.
    stage_pdfium(archive, target, profile_dir / "deps")
    print(f"Staged PDFium in {profile_dir}")


def copy_documentation(destination):
    shutil.copy2(ROOT / "README.md", destination / "README.md")
    shutil.copytree(ROOT / "docs", destination / "docs")


def write_mac_bundle(app, executable, archive, target, version, output):
    contents = app / "Contents"
    mac_os = contents / "MacOS"
    resources = contents / "Resources"
    mac_os.mkdir(parents=True)
    shutil.copy2(executable, mac_os / executable.name)
    library = stage_pdfium(
        archive, target, contents / "Frameworks", resources / "licenses" / "PDFium"
    )
    copy_documentation(resources)
    shutil.copy2(MAC_ICON, resources / MAC_ICON.name)
    with (contents / "Info.plist").open("wb") as plist:
        plistlib.dump({
            "CFBundleDevelopmentRegion": "ja",
            "CFBundleDisplayName": "Elafra",
            "CFBundleExecutable": executable.name,
            "CFBundleIdentifier": "app.elafra.Elafra",
            "CFBundleIconFile": MAC_ICON.name,
            "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundleName": "Elafra",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": version,
            "LSMinimumSystemVersion": "15.0",
            "NSHighResolutionCapable": True,
        }, plist)
    # Apple Silicon needs a valid code signature. Ad-hoc signing needs no identity.
    subprocess.run(["codesign", "--force", "--sign", "-", library], check=True)
    subprocess.run(["codesign", "--force", "--sign", "-", app], check=True)
    subprocess.run(["codesign", "--verify", "--deep", "--strict", app], check=True)
    subprocess.run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", app, output], check=True)


def linux_tar_info(info):
    info.mode = 0o755 if info.isdir() or info.name.endswith("/elafra") else 0o644
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    return info


def find_inno_compiler():
    if compiler := shutil.which("ISCC.exe"):
        return Path(compiler)
    for variable, folder in (("ProgramFiles(x86)", ""), ("LOCALAPPDATA", "Programs")):
        if root := os.environ.get(variable):
            compiler = Path(root) / folder / "Inno Setup 6" / "ISCC.exe"
            if compiler.is_file():
                return compiler
    raise ValueError("Inno Setup 6 with ISCC.exe is required to build the Windows installer")


def write_windows_installer(compiler, bundle, version, output):
    subprocess.run(
        [
            compiler,
            f"-dPackageVersion={version}",
            f"-dBundleDir={bundle}",
            f"-o{output.parent}",
            f"-f{output.stem}",
            ROOT / "scripts" / "windows-installer.iss",
        ],
        cwd=ROOT, check=True,
    )


def package(archive, target, target_dir, executable_dir, installer=False):
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    if installer and target != "win-x64":
        raise ValueError("--installer is only supported when packaging for Windows x64")
    compiler = find_inno_compiler() if installer else None
    build_env = dict(os.environ)
    if target.startswith("mac-"):
        build_env.setdefault("MACOSX_DEPLOYMENT_TARGET", "15.0")
    subprocess.run(
        ["cargo", "build", "--locked", "--release", "--target-dir", target_dir],
        cwd=ROOT, env=build_env, check=True,
    )
    executable_name = "elafra.exe" if target == "win-x64" else "elafra"
    executable = executable_dir / executable_name
    dist = ROOT / "dist"
    dist.mkdir(exist_ok=True)
    archive_name = f"elafra-{version}-{target}"
    extension = ".tar.gz" if target == "linux-x64" else ".zip"
    with tempfile.TemporaryDirectory(dir=dist) as temporary:
        temporary = Path(temporary)
        output = temporary / f"{archive_name}{extension}"
        setup = None
        if target.startswith("mac-"):
            write_mac_bundle(temporary / "Elafra.app", executable, archive, target, version, output)
        else:
            bundle = temporary / archive_name
            bundle.mkdir()
            shutil.copy2(executable, bundle / executable_name)
            stage_pdfium(archive, target, bundle, bundle / "licenses" / "PDFium")
            copy_documentation(bundle)
            if target == "win-x64":
                if compiler is not None:
                    setup = temporary / f"{archive_name}-setup.exe"
                    write_windows_installer(compiler, bundle, version, setup)
                (bundle / "portable.flag").touch()
                with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as zipped:
                    for file in sorted(bundle.rglob("*")):
                        if file.is_file():
                            zipped.write(file, file.relative_to(temporary).as_posix())
            else:
                with tarfile.open(output, "w:gz") as zipped:
                    zipped.add(bundle, arcname=bundle.name, filter=linux_tar_info)
        for artifact in (output, setup):
            if artifact is not None:
                destination = artifact.replace(dist / artifact.name)
                print(destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    stage = commands.add_parser("prepare", help="copy PDFium for local Cargo run and test")
    stage.add_argument("--profile", choices=["debug", "release"], default="debug")
    bundle = commands.add_parser("package", help="build the release and create a native archive in dist/")
    bundle.add_argument("--installer", action="store_true", help="also build the Windows installer")
    args = parser.parse_args()

    try:
        target, host = native_target()
        target_dir = (ROOT / (os.environ.get("CARGO_TARGET_DIR") or "target")).resolve()
        configured_target = os.environ.get("CARGO_BUILD_TARGET")
        if configured_target and configured_target != host:
            raise ValueError("PDFium staging and packaging require a native Cargo build target")
        profiles_dir = target_dir / host if configured_target else target_dir
        archive = fetch_pdfium(target, target_dir / "pdfium" / PDFIUM_VERSION)
        if args.command == "prepare":
            prepare(archive, target, profiles_dir / args.profile)
        else:
            package(archive, target, target_dir, profiles_dir / "release", installer=args.installer)
    except (OSError, ValueError, tarfile.TarError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Packaging failed: {error}\n")


if __name__ == "__main__":
    main()
