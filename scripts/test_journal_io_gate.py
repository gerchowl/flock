"""Mutation coverage for the journal's counted-I/O architecture gate."""
import tempfile
import unittest
from pathlib import Path

from scripts import journal_io_gate as gate


class JournalIoGate(unittest.TestCase):
    def test_native_paths_and_aliases_cannot_escape(self):
        for source in [
            '::std::fs::write(&path, &prefix)?;',
            'std :: fs :: write(&path, &prefix)?;',
            '::std::fs::write(path, bytes); // guardrails-ok',
            '#[cfg(not(test))] fn bypass() { ::std::fs::write(path, bytes); }',
            'tokio::fs::write(path, bytes).await?;',
            'use std::fs as native; native::write(path, bytes)?;',
            'use ::std::{fs as native};',
            'use std::{sync::Arc, fs::{File as Native}};',
            'use tokio::{fs::{self, File}};',
            'use std as native; native::fs::write(path, bytes)?;',
            'use r#std::r#fs as native;',
            'use std::fs::*;',
            'File::open(path)?;',
            'File::create(path)?;',
            'File::from_raw_fd(fd);',
            'OpenOptions::new().open(path)?;',
            'use crate::platform::structured as native;',
            'libc::write(fd, bytes, size);',
            'use std as filesystem; filesystem::fs::write(path, bytes)?;',
            'foreign::filesystem::fs::write(path, bytes)?;',
            'mod filesystem { pub use std::*; }',
            '#[path = "outside.rs"] mod extra;',
            '#[cfg_attr(feature = "web", path = "outside.rs")] mod extra;',
            'include!("outside.rs");',
        ]:
            with self.subTest(source=source):
                self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_counted_interfaces_and_literals_are_allowed(self):
        source = '''
mod filesystem;
use filesystem::{File, files, LockFile};
use super::filesystem;
filesystem::fs::read(path)?;
filesystem::rename(from, to)?;
filesystem::sync_directory(root)?;
files::open_append_file(path)?;
file.try_clone()?.write_all(bytes)?;
// ::std::fs::write(path, bytes)
/* outer /* tokio::fs */ OpenOptions */
let message = "::std::fs::write";
let raw = br###" OpenOptions \" std::fs "###;
let character = '\\'';
let lifetime: &'static str = "File::open";
'''
        self.assertEqual(gate.check_text(source, gate.COMMANDS / 'journal.rs'), [])

    def test_reports_actual_code_line_after_multiline_literals(self):
        source = 'let text = r#"first\nstd::fs::write\n"#;\n::std::fs::write(path, bytes);'
        self.assertEqual(gate.check_text(source, gate.COMMANDS / 'journal.rs')[0][0], 4)

    def test_nested_and_new_modules_and_ownership_are_scanned(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ['journal.rs', 'ownership.rs', 'journal_extra.rs',
                         'journal/nested/new.rs', 'journal/nested/filesystem.rs']:
                path = root / gate.COMMANDS / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('::std::fs::write(path, bytes)?;')
            boundary = root / gate.BOUNDARY
            boundary.write_text('::std::fs::write(path, bytes)?;')
            findings = gate.check_tree(root)
            self.assertEqual(len(findings), 5)
            self.assertTrue(any('ownership.rs:' in finding for finding in findings))
            self.assertTrue(any('nested/filesystem.rs:' in finding for finding in findings))

    def test_missing_journal_anchor_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = root / gate.COMMANDS
            commands.mkdir(parents=True)
            (commands / 'ownership.rs').write_text('')
            with self.assertRaises(FileNotFoundError):
                gate.check_tree(root)

    def test_boundary_cannot_be_redeclared_in_a_child(self):
        self.assertTrue(gate.check_text('mod filesystem;', gate.COMMANDS / 'journal/new.rs'))

    def test_repository_uses_counted_boundary(self):
        self.assertEqual(gate.check_tree(gate.ROOT), [])


if __name__ == '__main__':
    unittest.main()
