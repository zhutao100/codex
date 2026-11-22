import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch

from install_native_deps import _download_artifacts


class ReleaseArtifactsTest(unittest.TestCase):
    def test_downloads_from_the_producing_fork(self):
        with patch.object(subprocess, "check_call") as download:
            _download_artifacts(
                "https://github.com/example/codex/actions/runs/123/attempts/2",
                Path("out"),
            )
        download.assert_called_once_with(
            ["gh", "run", "download", "--dir", "out", "--repo", "example/codex", "123"]
        )

    def test_does_not_guess_a_repository_from_an_invalid_url(self):
        with self.assertRaises(ValueError), patch.object(subprocess, "check_call") as download:
            _download_artifacts("https://github.com/example/codex/releases/tag/123", Path("out"))
        download.assert_not_called()
