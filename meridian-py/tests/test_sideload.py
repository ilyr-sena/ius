import io
import plistlib
from pathlib import Path
from meridian_py.sideload.signer import find_zsign
from meridian_py.sideload.profile import parse_mobileprovision, key_matches_cert
from meridian_py.sideload import read_ipa_metadata


def test_find_zsign():
    z = find_zsign()
    assert z.exists()


def test_parse_mobileprovision():
    sample_plist = {"Name": "Test Profile", "TeamIdentifier": ["SRTHYBYH35"], "Version": 1}
    body = plistlib.dumps(sample_plist)
    # Wrap in dummy CMS header/trailer
    fake_prov = b"PREFIX_CMS_DATA" + body + b"SUFFIX_CMS_DATA"
    parsed = parse_mobileprovision(fake_prov)
    assert parsed["Name"] == "Test Profile"
    assert parsed["TeamIdentifier"] == ["SRTHYBYH35"]


def test_read_ipa_metadata():
    ipa = Path("/home/sooku/ipa-share/IUSProbe-unsigned.ipa")
    if ipa.exists():
        bid, name = read_ipa_metadata(ipa)
        assert bid == "dev.ius.probe.app"
        assert name == "IUSProbe"
