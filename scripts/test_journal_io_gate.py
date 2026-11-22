"""Mutation coverage for the journal's counted-I/O architecture gate."""
import tempfile
import unittest
from unittest.mock import patch
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
use self::filesystem;
use filesystem::fs;
filesystem::sync_directory(root)?;
files::open_append_file(path)?;
impl<R: Read> Read for ReplayReader<R> {
    fn read(&mut self) { self.source.read(bytes)?; }
}
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
            self.assertEqual(len({finding.split(":")[0] for finding in findings}), 5)
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

    def test_sibling_reexport_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = root / gate.COMMANDS
            commands.mkdir(parents=True)
            (commands / 'ownership.rs').write_text('')
            (commands / 'sibling.rs').write_text(
                'pub use std::fs::write as helper;')
            (commands / 'journal.rs').write_text(
                'use super::sibling::helper; fn bypass() { helper(path, bytes); }')
            self.assertTrue(gate.check_tree(root))

    def test_unreviewed_imports_paths_and_macros_are_rejected(self):
        for source in [
            'use crate::other::helper;',
            'use crate::r#sibling::r#helper as renamed;',
            'use super::ownership::helper;',
            'pub use super::{sibling::{helper as renamed}};',
            'use ::crate::other::helper as renamed;',
            'crate::other::helper(path, bytes);',
            '::crate::other::helper(path, bytes);',
            'return ::crate::other::helper(path, bytes);',
            'return ::other::helper(path, bytes);',
            'super::sibling::helper(path, bytes);',
            'use std::process::Command as Launcher;',
            '::std::process::Command::new("program");',
            'use std::sync::*;',
            'use crate::web::wire::commands::*;',
            'use super::*;',
            'use std as native;',
            'use std::sync as shared; use shared::*;',
            'crate::other::write!(path);',
            '::other::write!(path);',
            'write_from_sibling!(path);',
            '#[write_from_sibling] fn bypass() {}',
            '#[cfg_attr(test, write_from_sibling)] fn bypass() {}',
            '#[derive(WriteFromSibling)] struct Data;',
            'use filesystem::std as native; native::os::fd::OwnedFd::from(value);',
            'use self::filesystem::std::os::fd::OwnedFd;',
            'filesystem::std::os::fd::OwnedFd::from(value);',
            'extern crate other as helper;',
            '#[macro_use] mod extra;',
            'mod nested { use super::super::sibling::helper; }',
            'mod nested { use super::super::*; }',
            'mod nested { ::crate::other::helper(); }',
        ]:
            with self.subTest(source=source):
                self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_reviewed_imports_relative_paths_and_macros_are_allowed(self):
        source = '''
use crate::web::wire::commands::{Command as Input, ErrorCode};
use ::std::{sync::{Arc as Shared}, path::PathBuf};
use serde_json::json as payload;
#[derive(serde::Serialize)]
struct Data;
crate::logging::client_journal_replay("ready", 1, 1);
::std::sync::Arc::new(value);
serde_json::json!({});
payload!({});
mod nested {
    use super::{filesystem, Command};
    use super::super::ErrorCode;
    super::super::ack(id, result);
}
'''
        self.assertEqual(gate.check_text(source, gate.COMMANDS / 'journal.rs'), [])

    def test_symlinked_modules_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = root / gate.COMMANDS
            journal = commands / 'journal'
            journal.mkdir(parents=True)
            (commands / 'journal.rs').write_text('mod external;')
            (commands / 'ownership.rs').write_text('')
            outside = root / 'outside'
            outside.mkdir()
            (outside / 'mod.rs').write_text('pub use std::fs::write as helper;')
            (journal / 'external').symlink_to(outside, target_is_directory=True)
            self.assertTrue(gate.check_tree(root))

    def test_crate_macro_shadowing_is_rejected(self):
        for name in ['println', 'json']:
            for location in ['src/main.rs', 'src/web/wire/commands/sibling.rs']:
                with self.subTest(name=name, location=location):
                    with tempfile.TemporaryDirectory() as directory:
                        root = Path(directory)
                        commands = root / gate.COMMANDS
                        commands.mkdir(parents=True)
                        (commands / 'journal.rs').write_text(name + '!("payload");')
                        (commands / 'ownership.rs').write_text('')
                        definition = root / location
                        definition.parent.mkdir(parents=True, exist_ok=True)
                        definition.write_text(
                            '#[macro_export] macro_rules! ' + name
                            + ' { ($($arg:tt)*) => { std::fs::write(path, bytes) } }')
                        self.assertTrue(gate.check_tree(root))

    def test_filesystem_and_process_capabilities_are_not_general(self):
        for source in [
            'std::path::Path::read_dir(path);',
            'use std::path::Path; Path::read_dir(path);',
            'use std::path::Path as Input; Input::metadata(path);',
            'crate::structured::launch::SessionConfigDir::create(path);',
            'crate::process::TracedCommand::new("program").status();',
            'use crate::process::TracedCommand as Runner; Runner::new("program");',
            'use crate::structured::launch::SessionConfigDir as Dir; Dir::create(path);',
        ]:
            with self.subTest(source=source):
                self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_new_internal_globs_are_rejected(self):
        for path in ['journal.rs', 'journal/compact.rs', 'journal/tests.rs']:
            with self.subTest(path=path):
                self.assertTrue(gate.check_text(
                    'mod x {} use self::x::*;', gate.COMMANDS / path))

    def test_test_capabilities_require_test_file_paths(self):
        source = """
use crate::structured::launch::SessionConfigDir as Directory;
use crate::process::TracedCommand as Runner;
Directory::create(path);
Runner::new("program").status();
std::process::exit(0);
"""
        for relative in ['journal/tests.rs', 'journal/tests/compaction.rs', 'ownership/tests.rs']:
            with self.subTest(relative=relative):
                self.assertEqual(gate.check_text(source, gate.COMMANDS / relative), [])
        for relative in ['journal.rs', 'journal/helpers.rs', 'journal/tests_extra.rs']:
            with self.subTest(relative=relative):
                self.assertTrue(gate.check_text(source, gate.COMMANDS / relative))
        self.assertTrue(gate.check_text(
            '#[cfg(test)] mod fixtures { ' + source + ' }', gate.COMMANDS / 'journal.rs'))

    def test_test_subtree_entry_requires_cfg_test(self):
        for source in ['mod tests;', 'mod tests {}', '#[cfg(any(test, feature="web"))] mod tests;']:
            with self.subTest(source=source):
                self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))
        self.assertEqual(gate.check_text(
            '#[cfg(test)] mod tests;', gate.COMMANDS / 'journal.rs'), [])

    def test_existing_glob_permissions_are_exact_and_single_use(self):
        for path in ['journal/compact.rs', 'journal/tests.rs', 'journal/tests/capacity.rs',
                     'journal/tests/compaction.rs', 'ownership/tests.rs']:
            with self.subTest(path=path):
                self.assertEqual(gate.check_text('use super::*;', gate.COMMANDS / path), [])
                self.assertTrue(gate.check_text(
                    'use super::*; use super::*;', gate.COMMANDS / path))
                self.assertTrue(gate.check_text(
                    'mod nested { use super::*; }', gate.COMMANDS / path))
                self.assertTrue(gate.check_text(
                    'fn nested() { use super::*; }', gate.COMMANDS / path))

    def test_path_permissions_do_not_grant_filesystem_members(self):
        for source in [
            'Path::read_dir(path);',
            '<std::path::Path>::read_dir(path);',
            'use std::path::Path as Alias; <Alias>::read_dir(path);',
            'PathBuf::metadata(path);',
            'use super::Path as Input; Input::read_dir(path);',
            'use self::PathBuf as Input; Input::metadata(path);',
            'use std::path::{Path as Input}; Input::read_link(path);',
            'use std::io as streams; streams::stdout();',
            'io::stdout();',
        ]:
            with self.subTest(source=source):
                self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))
        self.assertEqual(gate.check_text(
            'use std::path::{Path, PathBuf}; Path::new(value);', gate.COMMANDS / 'journal.rs'), [])

    def test_in_tree_macros_are_checked_and_cannot_shadow_reviewed_names(self):
        path = gate.COMMANDS / 'journal.rs'
        self.assertEqual(gate.check_text(
            'macro_rules! safe { () => { 1 } } safe!();', path), [])
        self.assertTrue(gate.check_text(
            'macro_rules! hidden { () => { std::fs::write(path, bytes) } } hidden!();', path))
        self.assertTrue(gate.check_text(
            'macro_rules! println { () => { 1 } } println!();', path))
        self.assertTrue(gate.check_text(
            'macro_rules! json { () => { 1 } } serde_json::json!();', path))

    def test_macro_namespace_injection_and_raw_shadowing_are_rejected(self):
        for definition in [
            'macro_rules! make { ($name:ident) => { macro_rules! $name { () => { 1 } } } } make!(println);',
            '#[macro_use] extern crate external;',
            '#![macro_use] extern crate external;',
            'macro_rules! r#println { () => { 1 } }',
        ]:
            with self.subTest(definition=definition):
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    commands = root / gate.COMMANDS
                    commands.mkdir(parents=True)
                    (commands / 'journal.rs').write_text('println!("message");')
                    (commands / 'ownership.rs').write_text('')
                    (root / 'src/main.rs').write_text(definition)
                    self.assertTrue(gate.check_tree(root))
                    (root / 'src/main.rs').write_text('// ' + definition)
                    self.assertEqual(gate.check_tree(root), [])

    def test_receiver_typed_filesystem_probe_is_rejected(self):
        source = 'fn probe() { let _ = std::path::Path::new(".").read_dir(); }'
        self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_type_alias_and_late_import_probes_are_rejected(self):
        for source in [
            'type InputPath = std::path::Path; fn probe() { InputPath::read_dir(path); }',
            'fn probe() { InputPath::read_dir(path); } use std::path::Path as InputPath;',
        ]:
            with self.subTest(source=source):
                self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_method_layer_refuses_any_receiver_and_associated_type(self):
        methods = """read_dir metadata symlink_metadata exists try_exists is_file is_dir
is_symlink canonicalize read_link read_to_string read write create create_new open
remove_file remove_dir remove_dir_all create_dir create_dir_all rename copy hard_link
set_permissions spawn output status""".split()
        for name in methods:
            for source in ['object.' + name + '(value);', 'Input::' + name + '(value);']:
                with self.subTest(source=source):
                    self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))
        for source in ['let operation = Unknown::read_dir; operation(value);',
                       'object.r#read_dir();', 'object.read::<u8>(value);',
                       'Input::write::<u8>(value);', 'object.read("literal ) { text");',
                       'object.read::<u8>;']:
            with self.subTest(source=source):
                self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_alias_resolution_is_independent_of_the_method_layer(self):
        for source in [
            'fn probe() { InputPath::unreviewed(); } type InputPath = std::path::Path;',
            'fn probe() { InputPath::unreviewed(); } use std::path::Path as InputPath;',
            'fn probe() { InputPath::unreviewed(); } type InputPath = Other; type Other = std::path::Path;',
            'fn probe() { InputPath::unreviewed(); } use Other as InputPath; use std::path::Path as Other;',
            'types::Input::unreviewed(); mod types { type Input = std::path::Path; }',
            'fn probe() { self::Input::unreviewed(); } type Input = std::path::Path;',
            'type One = Two; type Two = One; One::new(value);',
        ]:
            with self.subTest(source=source):
                self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))
        for source in [
            'fn probe() { Shared::new(value); } use std::sync::Arc as Shared;',
            'fn probe() { InputPath::new(value); } type InputPath = std::path::Path;',
            'fn probe() { InputPath::new(value); } type InputPath = Other; use std::path::Path as Other;',
            'type Input = std::path::Path; mod inner { type Input = std::sync::Arc; fn probe() { Input::new(value); } }',
            'types::Input::new(value); mod types { type Input = std::path::Path; }',
        ]:
            with self.subTest(source=source):
                self.assertEqual(gate.check_text(source, gate.COMMANDS / 'journal.rs'), [])

    def test_call_exceptions_require_file_item_receiver_and_method(self):
        path = gate.COMMANDS / 'journal.rs'
        allowed = 'impl<R: Read> Read for ReplayReader<R> { fn read() { self.source.read(other); } }'
        self.assertEqual(gate.check_text(allowed, path), [])
        for source, other_path in [
            (allowed, gate.COMMANDS / 'journal/compact.rs'),
            (allowed.replace('ReplayReader<R>', 'Other<R>'), path),
            (allowed.replace('fn read()', 'fn unrelated()'), path),
            (allowed.replace('self.source', 'other.source'), path),
            (allowed.replace('.read(other)', '.write(other)'), path),
            ('mod nested { ' + allowed + ' }', path),
            ('self.source.read(bytes);', path),
            ('fn read() { self.source.read(bytes); }', path),
            (allowed.replace('self.source.read(other);',
                             'fn read() { self.source.read(other); }'), path),
        ]:
            with self.subTest(source=source, path=other_path):
                self.assertTrue(gate.check_text(source, other_path))
        self.assertEqual(gate.check_text(allowed.replace('other', 'changed'), path), [])
        self.assertEqual(gate.check_text(allowed.replace('self.source.read(other);',
            'self /* receiver */ . source . read(other); self.source.read(again);'), path), [])

    def test_deleted_pinned_call_cannot_authorize_another_item(self):
        path = gate.COMMANDS / 'journal.rs'
        source = (gate.ROOT / path).read_text()
        source = source.replace('file.metadata()', 'file.reviewed_metadata()', 1)
        self.assertEqual(gate.check_text(source, path), [])
        source += '\nfn unrelated(file: &Path) { file.metadata(); }\n'
        self.assertTrue(gate.check_text(source, path))

    def test_macro_method_substitution_is_rejected(self):
        source = ('macro_rules! probe { ($p:expr, $method:ident) => { $p.$method() } } '
                  'probe!(path, read_dir);')
        self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))
        for left, right in [('(', ')'), ('[', ']')]:
            with self.subTest(delimiter=left):
                definition = ('macro_rules! probe ' + left
                              + ' ($p:expr, $method:ident) => { $p.$method() } '
                              + right + '; probe!(path, read_dir);')
                self.assertTrue(gate.check_text(definition, gate.COMMANDS / 'journal.rs'))

    def test_macro_path_repetition_is_rejected(self):
        source = ('macro_rules! review_path { ($($p:ident),*) => { $($p)::* } } '
                  'review_path!(Path, read_dir)(path);')
        self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_macro_method_repetition_is_rejected(self):
        source = ('macro_rules! review_method { ($($m:ident),*) => { $($m()).* } } '
                  'review_method!(make_path, read_dir);')
        self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_repetition_positions_fail_closed_and_allowlist_is_exact(self):
        path = gate.COMMANDS / 'journal.rs'
        for body in ['$($p)::*', '$($p)::+', '$($p()).*', '$(.$m())*',
                     '$($p),*::operation()', '$($p),*.operation()',
                     'object.$($p)*()', 'module::$($p)*()',
                     '$($p::operation()),*', '$($($p)::*),*',
                     '$($p::operation())?', '$(fixed::operation($p)),*']:
            with self.subTest(body=body):
                source = 'macro_rules! probe { ($($p:ident),*) => { ' + body + ' } }'
                code = gate.tokens(source)
                self.assertTrue(gate.macro_position_findings(code, path))
                self.assertTrue(gate.check_text(source, path))
                with patch.dict(gate.MACRO_POSITION_ALLOWLIST,
                                {(path, 'probe'): 'reviewed repetition fixture'}):
                    self.assertEqual(gate.macro_position_findings(code, path), [])
                    self.assertTrue(gate.macro_position_findings(
                        code, gate.COMMANDS / 'journal/compact.rs'))
        for body in ['$($p),*', '$($($p),*);*', '[$($p)*]',
                     '$($p),* + 1', '$($p),* ":: ."',
                     '$($p),* /* :: . */']:
            with self.subTest(body=body):
                self.assertEqual(gate.macro_position_findings(gate.tokens(
                    'macro_rules! safe { ($($p:ident),*) => { ' + body + ' } }'), path), [])

    def test_macro_path_positions_require_explicit_review(self):
        path = gate.COMMANDS / 'journal.rs'
        for body in ['$p::helper()', '$crate::helper::read_dir(path)',
                     'path.$method::<u8>()', '$crate::helper::metadata',
                     '$crate::$method(path)', '$crate::helper::$method(path)',
                     'helper::$method(path)']:
            with self.subTest(body=body):
                source = 'macro_rules! probe { ($p:ident) => { ' + body + ' } }'
                code = gate.tokens(source)
                self.assertTrue(gate.macro_position_findings(code, path))
                self.assertTrue(gate.check_text(source, path))
        for body in ['$value', '$crate::web::wire::commands::ack(id, result)',
                     '($($value),*)', '".$method() $p::read_dir()"']:
            with self.subTest(body=body):
                self.assertEqual(gate.macro_position_findings(gate.tokens(
                    'macro_rules! safe { ($value:expr) => { ' + body + ' } }'), path), [])
        with patch.dict(gate.MACRO_POSITION_ALLOWLIST,
                        {(path, 'probe'): 'fixture for explicit macro review'}):
            self.assertEqual(gate.macro_position_findings(gate.tokens(
                'macro_rules! probe { () => { path.$method() } }'), path), [])
            self.assertEqual(gate.check_text(
                'macro_rules! probe { ($method:ident) => { path.$method() } }', path), [])
        # Generated functions cannot inherit a real function's exception.
        for wrapper in ['macro_rules! probe { () => { %s } }',
                        'const NESTED: () = { %s };', 'const _: () = const { %s };',
                        'static mut NESTED: () = { %s };']:
            with self.subTest(wrapper=wrapper):
                self.assertTrue(gate.check_text(
                    wrapper % 'fn private() { file.sync_all(); }', path))

    def test_non_risky_arguments_and_closure_edits_pass(self):
        path = gate.COMMANDS / 'journal.rs'
        source = (gate.ROOT / path).read_text()
        edited = source.replace('journal.replay_advanced(0);', 'journal.replay_advanced(1);')
        edited = edited.replace('self.file.set_len(good)', 'self.file.set_len(other)')
        self.assertNotEqual(source, edited)
        self.assertEqual(gate.check_text(edited, path), [])
        self.assertTrue(gate.check_text(edited.replace(
            '.name("wire-journal".into())', '.name("different-worker".into())'), path))
        self.assertTrue(gate.check_text(edited.replace('journal.replay_advanced(1);',
            'journal.replay_advanced(1); unreviewed.read_dir();'), path))

    def test_item_paths_distinguish_impls_modules_and_preserve_closures(self):
        source = ("impl<T> Trait for Thing<T> { fn read(x: fn()) { "
                  "let task = || { self.source.read(x); }; } } "
                  "mod nested { impl Other { fn read() { self.source.read(x); } } }")
        code = gate.tokens(source)
        names = [name for name, _ in code]
        items = gate.item_paths(names, gate.delimiter_pairs(names))
        paths = [items[i] for i, name in enumerate(names) if name == 'source']
        self.assertEqual(paths, [('impl < T > Trait for Thing < T >', 'fn read'),
                                 ('mod nested', 'impl Other', 'fn read')])

    def test_method_layer_skips_only_boundary_and_path_enforced_test_files(self):
        for relative in ['journal/filesystem.rs', 'journal/tests.rs', 'journal/tests/compaction.rs']:
            with self.subTest(relative=relative):
                self.assertEqual(gate.check_text('object.read_dir();', gate.COMMANDS / relative), [])
        self.assertTrue(gate.check_text(
            '#[cfg(test)] fn probe() { object.read_dir(); }', gate.COMMANDS / 'journal.rs'))
        self.assertEqual(gate.check_text(
            '// object.read_dir();\nlet text = "object.read_dir()"; fn read_dir() {}',
            gate.COMMANDS / 'journal.rs'), [])

    def test_repository_uses_counted_boundary(self):
        self.assertEqual(gate.check_tree(gate.ROOT), [])


if __name__ == '__main__':
    unittest.main()
