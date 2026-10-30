Kindling repair.rs, vendored unchanged from https://github.com/ciscoriordan/kindling/blob/3b91b1ea21f892779be09149f21fe4053b1b3569/src/repair.rs (MIT). Original repairs derive from innocenat/kindle-epub-fix (public domain).

The surrounding EPUB wrapper validates archive limits, normalizes declared encodings, repairs container rootfile media type, and supplies the selected language before calling this module. No DRM removal is included.

CWA container.xml compatibility behavior was independently implemented with attribution to https://github.com/crocodilestick/Calibre-Web-Automated/blob/main/scripts/kindle_epub_fixer.py . CWA fixture provenance is recorded separately in tests/fixtures/cwa/README.md.
