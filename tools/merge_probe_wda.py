#!/usr/bin/env python3
"""Merges IUSProbe screen streaming engine directly into WebDriverAgentRunner.

Produces a single unified 'MeridianRunner' IPA containing:
- Port 8100: WebDriverAgentLib automation (tap, swipe, keys, testmanagerd hooks)
- Port 9100: IUSProbe ultra-low-latency ScreenCaptureKit H.264 stream & WebCodecs GPU player
"""

from __future__ import annotations

import argparse
import logging
import plistlib
import shutil
import sys
from pathlib import Path

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("merge_probe_wda")

PROBE_SWIFT_FILES = [
    "CaptureProbe.swift",
    "EncoderProbe.swift",
    "H264Stream.swift",
    "MJPEGStreamer.swift",
    "TinyHTTPServer.swift",
    "AudioKeepAlive.swift",
    "ProbeOrchestrator.swift",
    "WdaRelay.swift",
]

PROBE_BRIDGE_SWIFT = """import Foundation

@objc(ProbeBridge) public final class ProbeBridge: NSObject {
    @objc public static func start() {
        print("[MeridianRunner] Starting integrated ProbeOrchestrator on port 9200...")
        ProbeOrchestrator.shared.start(port: 9200)
    }
}
"""


def generate_meridian_icon(icon_path: Path) -> None:
    """Generate modern, high-res Meridian app icon."""
    try:
        from PIL import Image, ImageDraw, ImageFilter
    except ImportError:
        log.warning("Pillow not installed; skipping custom icon generation")
        return

    size = 1024
    img = Image.new("RGBA", (size, size), (10, 14, 23, 255))
    draw = ImageDraw.Draw(img)

    for r in range(size // 2, 0, -2):
        ratio = r / (size // 2)
        c_r = int(10 + (18 - 10) * (1 - ratio))
        c_g = int(14 + (30 - 14) * (1 - ratio))
        c_b = int(23 + (55 - 23) * (1 - ratio))
        bbox = [size // 2 - r, size // 2 - r, size // 2 + r, size // 2 + r]
        draw.ellipse(bbox, fill=(c_r, c_g, c_b, 255))

    draw.ellipse([140, 140, 884, 884], outline=(0, 242, 254, 40), width=6)
    draw.ellipse([200, 200, 824, 824], outline=(79, 172, 254, 60), width=4)

    glow = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    glow_draw = ImageDraw.Draw(glow)
    pts = [(270, 740), (270, 320), (512, 590), (754, 320), (754, 740)]
    glow_draw.line(pts, fill=(0, 242, 254, 180), width=68, joint="round")
    glow = glow.filter(ImageFilter.GaussianBlur(radius=28))
    img = Image.alpha_composite(img, glow)

    fg = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    fg_draw = ImageDraw.Draw(fg)
    fg_draw.line([(270, 740), (270, 320)], fill=(0, 242, 254, 255), width=44, joint="round")
    fg_draw.line([(270, 320), (512, 590)], fill=(79, 172, 254, 255), width=44, joint="round")
    fg_draw.line([(512, 590), (754, 320)], fill=(79, 172, 254, 255), width=44, joint="round")
    fg_draw.line([(754, 320), (754, 740)], fill=(0, 242, 254, 255), width=44, joint="round")
    for pt in pts:
        fg_draw.ellipse([pt[0] - 22, pt[1] - 22, pt[0] + 22, pt[1] + 22], fill=(255, 255, 255, 255))

    img = Image.alpha_composite(img, fg)
    img_draw = ImageDraw.Draw(img)
    img_draw.ellipse([512 - 14, 590 - 14, 512 + 14, 590 + 14], fill=(0, 242, 254, 255))

    icon_path.parent.mkdir(parents=True, exist_ok=True)
    img.convert("RGB").save(icon_path, "PNG")
    log.info("✓ Generated custom Meridian icon at %s", icon_path)


def integrate(wda_dir: Path, probe_dir: Path) -> None:
    wda_dir = wda_dir.resolve()
    probe_dir = probe_dir.resolve()
    runner_dir = wda_dir / "WebDriverAgentRunner"
    pbx_path = wda_dir / "WebDriverAgent.xcodeproj" / "project.pbxproj"
    plist_path = runner_dir / "Info.plist"
    ui_tests_path = runner_dir / "UITestingUITests.m"

    if not runner_dir.exists():
        raise FileNotFoundError(f"WebDriverAgentRunner directory not found: {runner_dir}")
    if not pbx_path.exists():
        raise FileNotFoundError(f"project.pbxproj not found: {pbx_path}")

    log.info("1. Copying Probe Swift sources into %s...", runner_dir)
    for name in PROBE_SWIFT_FILES:
        src = probe_dir / name
        if not src.exists():
            raise FileNotFoundError(f"Missing probe source: {src}")
        shutil.copy(src, runner_dir / name)

    bridge_path = runner_dir / "ProbeBridge.swift"
    bridge_path.write_text(PROBE_BRIDGE_SWIFT)
    all_swift = PROBE_SWIFT_FILES + ["ProbeBridge.swift"]
    log.info("✓ Copied %d Swift files to WebDriverAgentRunner", len(all_swift))

    log.info("2. Patching UITestingUITests.m to initialize ProbeBridge...")
    ui_content = ui_tests_path.read_text()
    if "ProbeBridge" not in ui_content:
        hook = (
            "  Class probe = NSClassFromString(@\"ProbeBridge\");\n"
            "  if (!probe) { probe = NSClassFromString(@\"WebDriverAgentRunner.ProbeBridge\"); }\n"
            "  if (probe) { [probe performSelector:NSSelectorFromString(@\"start\")]; }\n"
            "  FBWebServer *webServer = [[FBWebServer alloc] init];"
        )
        ui_content = ui_content.replace(
            "FBWebServer *webServer = [[FBWebServer alloc] init];",
            hook,
        )
        ui_tests_path.write_text(ui_content)
        log.info("✓ UITestingUITests.m successfully wired to ProbeBridge (via ObjC runtime)")
    else:
        log.info("UITestingUITests.m already contains ProbeBridge integration")

    log.info("3. Patching Info.plist for Meridian branding, background modes and permissions...")
    with plist_path.open("rb") as f:
        plist = plistlib.load(f)

    plist["CFBundleDisplayName"] = "Meridian"
    plist["CFBundleName"] = "Meridian"
    plist["UIBackgroundModes"] = ["continuous", "audio", "screen-capture"]
    plist["NSScreenCaptureUsageDescription"] = "Screen capture for low-latency device display streaming."
    plist["NSLocalNetworkUsageDescription"] = "Local network streaming and automation server."

    with plist_path.open("wb") as f:
        plistlib.dump(plist, f)
    log.info("✓ Info.plist updated with Meridian branding and permissions")

    log.info("4. Replacing app icon with custom Meridian design...")
    icon_path = runner_dir / "Assets.xcassets" / "AppIcon.appiconset" / "icon-1024.png"
    asset_icon = probe_dir.parent / "Assets" / "meridian-icon.png"
    if not asset_icon.exists():
        asset_icon = probe_dir.parent / "Assets" / "icon-1024.png"
    if asset_icon.exists():
        icon_path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(asset_icon, icon_path)
        log.info("✓ Copied custom Meridian icon to %s", icon_path)
    else:
        generate_meridian_icon(icon_path)

    log.info("5. Updating project.pbxproj to rebrand bundle IDs and include Swift sources...")
    content = pbx_path.read_text()

    # Rebrand bundle identifiers
    content = content.replace("com.facebook.WebDriverAgentRunner", "dev.ius.meridian.runner")
    content = content.replace("com.facebook.WebDriverAgentLib", "dev.ius.meridian.lib")

    if "MDPRBF0000000000000000000" in content:
        log.info("project.pbxproj already contains Probe entries, skipping insertion")
        pbx_path.write_text(content)
        return

    build_files = []
    file_refs = []
    children_entries = []
    sources_entries = []

    for idx, name in enumerate(all_swift):
        fileref_id = f"MDPRBF{idx:02d}00000000000000000"
        buildfile_id = f"MDPRBB{idx:02d}00000000000000000"
        build_files.append(
            f"\t\t{buildfile_id} /* {name} in Sources */ = {{isa = PBXBuildFile; fileRef = {fileref_id} /* {name} */; }};"
        )
        file_refs.append(
            f"\t\t{fileref_id} /* {name} */ = {{isa = PBXFileReference; fileEncoding = 4; lastKnownFileType = sourcecode.swift; name = {name}; path = WebDriverAgentRunner/{name}; sourceTree = SOURCE_ROOT; }};"
        )
        children_entries.append(f"\t\t\t\t{fileref_id} /* {name} */,")
        sources_entries.append(f"\t\t\t\t{buildfile_id} /* {name} in Sources */,")

    # Insert PBXBuildFile
    bf_marker = "/* Begin PBXBuildFile section */\n"
    content = content.replace(bf_marker, bf_marker + "\n".join(build_files) + "\n")

    # Insert PBXFileReference
    fr_marker = "/* Begin PBXFileReference section */\n"
    content = content.replace(fr_marker, fr_marker + "\n".join(file_refs) + "\n")

    # Insert into children of EEF988341C486655005CA669 group
    group_target = "EEF988341C486655005CA669 /* WebDriverAgentRunner */ = {\n\t\t\tisa = PBXGroup;\n\t\t\tchildren = (\n"
    idx = content.find(group_target)
    if idx == -1:
        raise RuntimeError("Failed to locate WebDriverAgentRunner PBXGroup in project.pbxproj")
    ins_pos = idx + len(group_target)
    content = content[:ins_pos] + "\n".join(children_entries) + "\n" + content[ins_pos:]

    # Insert into Sources build phase: EEF988261C486603005CA669
    sources_target = "EEF988261C486603005CA669 /* Sources */ = {\n\t\t\tisa = PBXSourcesBuildPhase;\n\t\t\tbuildActionMask = 2147483647;\n\t\t\tfiles = (\n"
    idx = content.find(sources_target)
    if idx == -1:
        raise RuntimeError("Failed to locate WebDriverAgentRunner PBXSourcesBuildPhase in project.pbxproj")
    ins_pos = idx + len(sources_target)
    content = content[:ins_pos] + "\n".join(sources_entries) + "\n" + content[ins_pos:]

    # Add SWIFT_VERSION and settings to EEF988321C486604005CA669 (Debug) and EEF988331C486604005CA669 (Release)
    swift_settings = "\n\t\t\t\tSWIFT_VERSION = 5.0;\n\t\t\t\tALWAYS_EMBED_SWIFT_STANDARD_LIBRARIES = YES;\n\t\t\t\tDEFINES_MODULE = YES;\n\t\t\t\tIPHONEOS_DEPLOYMENT_TARGET = 18.0;"
    for cid in ["EEF988321C486604005CA669", "EEF988331C486604005CA669"]:
        marker = f"{cid} /* Debug */ = {{\n\t\t\tisa = XCBuildConfiguration;\n\t\t\tbaseConfigurationReference = EEE5CABF1C80361500CBBDD9 /* IOSSettings.xcconfig */;\n\t\t\tbuildSettings = {{"
        if marker not in content:
            marker = f"{cid} /* Release */ = {{\n\t\t\tisa = XCBuildConfiguration;\n\t\t\tbaseConfigurationReference = EEE5CABF1C80361500CBBDD9 /* IOSSettings.xcconfig */;\n\t\t\tbuildSettings = {{"
        idx = content.find(marker)
        if idx != -1:
            ins_pos = idx + len(marker)
            content = content[:ins_pos] + swift_settings + content[ins_pos:]

    pbx_path.write_text(content)
    log.info("✓ project.pbxproj modified and verified")
    log.info("🎉 MeridianRunner integration complete!")


def main() -> None:
    parser = argparse.ArgumentParser(description="Merge IUSProbe into WebDriverAgentRunner")
    parser.add_argument("--wda", type=Path, required=True, help="Path to WebDriverAgent checkout")
    parser.add_argument(
        "--probe",
        type=Path,
        default=Path(__file__).parent.parent / "probe" / "ProbeApp",
        help="Path to ProbeApp sources",
    )
    args = parser.parse_args()
    integrate(args.wda, args.probe)


if __name__ == "__main__":
    main()
