import tempfile
import unittest
from pathlib import Path

from phonelink.contacts import _parse_vcard_name, _parse_vcf_text


class VCardParseTests(unittest.TestCase):
    def test_folded_full_name_and_phone(self):
        entries = _parse_vcf_text(
            "BEGIN:VCARD\r\n"
            "VERSION:3.0\r\n"
            "FN:Jane\r\n"
            "  Doe\r\n"
            "TEL;TYPE=CELL:+1 (316) 555-1212\r\n"
            "END:VCARD\r\n"
        )

        self.assertEqual(entries, [("Jane Doe", ["13165551212"])])

    def test_quoted_printable_structured_name_fallback(self):
        entries = _parse_vcf_text(
            "BEGIN:VCARD\n"
            "VERSION:2.1\n"
            "N;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:Doe;Jos=C3=A9;;;\n"
            "TEL:+44 20 7946 0958\n"
            "END:VCARD\n"
        )

        self.assertEqual(entries, [("José Doe", ["442079460958"])])

    def test_escaped_full_name(self):
        entries = _parse_vcf_text(
            "BEGIN:VCARD\n"
            "FN:Doe\\, Jane\n"
            "TEL;TYPE=HOME:(316) 555-0000\n"
            "END:VCARD\n"
        )

        self.assertEqual(entries, [("Doe, Jane", ["3165550000"])])

    def test_parse_vcard_name_reads_first_entry_from_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "contacts.vcf"
            path.write_text(
                "BEGIN:VCARD\n"
                "FN:First Contact\n"
                "TEL:+1 555 111 2222\n"
                "END:VCARD\n"
                "BEGIN:VCARD\n"
                "FN:Second Contact\n"
                "TEL:+1 555 333 4444\n"
                "END:VCARD\n",
                encoding="utf-8",
            )

            self.assertEqual(_parse_vcard_name(path), ("First Contact", ["15551112222"]))


if __name__ == "__main__":
    unittest.main()
