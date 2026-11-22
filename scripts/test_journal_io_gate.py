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
fn private() { files::open_append_file(path)?; }
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
            self.assertEqual(len({finding.split(":")[0] for finding in findings}), 6)
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
use crate::structured::launch::SessionConfigDir;
use crate::process::TracedCommand;
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
        with patch.dict(gate.CRATE_MACRO_ALLOWLIST,
                        {(path, 'safe'): 'constant expansion cannot synthesize capability identifiers'}):
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

    def test_boundary_reexport_cannot_rename_capability(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = root / gate.COMMANDS
            (commands / 'journal').mkdir(parents=True)
            (commands / 'ownership.rs').write_text('')
            boundary = (gate.ROOT / gate.BOUNDARY).read_text()
            (root / gate.BOUNDARY).write_text(boundary.replace('remove_file,', 'remove_file as erase,'))
            (commands / 'journal.rs').write_text('fn probe() { filesystem::erase(path); }')
            self.assertTrue(gate.check_tree(root))

    def test_capability_import_alias_is_rejected(self):
        self.assertTrue(gate.check_text('use std::fs::read_dir as rd;', gate.COMMANDS / 'journal.rs'))

    def test_approved_alias_still_requires_anchored_use(self):
        path = gate.BOUNDARY
        source = (gate.ROOT / path).read_text().replace('remove_file,', 'remove_file as erase,')
        approved = {(path, (), ('::', 'std', 'fs', 'remove_file'), 'erase', True): 'fixture to test alias-use isolation',
                    (path, (), ('counted', 'std', 'fs', 'remove_file'), 'erase', True): 'fixture to test alias-use isolation'}
        with patch.object(gate, 'IMPORT_EXCEPTIONS',
                          getattr(gate, 'IMPORT_EXCEPTIONS', {}) | approved, create=True):
            self.assertEqual(gate.check_text(source, path), [])
            self.assertTrue(gate.check_text(source + '\nfn probe() { erase(path); }', path))

    def test_approved_alias_call_does_not_grant_another_use_or_reference(self):
        path = gate.COMMANDS / 'journal.rs'
        alias = (gate.BOUNDARY, (), ('::', 'std', 'fs', 'remove_file'), 'erase', True)
        call = (path, ('fn reviewed',), 'filesystem', 'erase')
        with patch.dict(gate.IMPORT_EXCEPTIONS, {alias: 'fixture for an approved renamed leaf'}):
            with patch.dict(gate.CALL_EXCEPTIONS, {call: 'fixture for exactly one alias use item'}):
                self.assertEqual(gate.check_text('fn reviewed() { filesystem::erase(path); }', path), [])
                self.assertTrue(gate.check_text('fn elsewhere() { filesystem::erase(path); }', path))
                self.assertTrue(gate.check_text(
                    'fn reviewed() { let other = filesystem::erase; other(path); }', path))

    def test_let_reference_cannot_reuse_a_call_anchor(self):
        for declaration in ['let erase = filesystem::remove_file;',
                            'let erase = (filesystem::remove_file);',
                            'let (erase,) = (filesystem::remove_file,);',
                            'let erase: fn(_) -> _ = filesystem::remove_file;']:
            source = 'impl Journal { fn start_compaction() { ' + declaration + ' erase(path); } }'
            with self.subTest(declaration=declaration):
                self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal/compact.rs'))

    def test_complete_import_leaf_pins_source_binding_and_root(self):
        path = gate.BOUNDARY
        original = (gate.ROOT / path).read_text()
        for before, after in [('remove_file,', 'remove_file as _,') ,
                              ('remove_file,', 'remove_file as r#erase,'),
                              ('remove_file,', 'remove_file as remove_file,'),
                              ('use ::std::fs::', 'use std::fs::'),
                              ('use ::std::fs::', 'use other::fs::')]:
            with self.subTest(after=after):
                source = original.replace(before, after)
                self.assertNotEqual(original, source)
                self.assertTrue(gate.check_text(source, path))

    def test_type_and_function_aliases_require_binding_and_use_permissions(self):
        path = gate.BOUNDARY
        declarations = [('type Native = File;', (), 'type', 'Native'),
                        ('const ERASE: fn() = safe;', ('const ERASE',), 'const', 'ERASE'),
                        ('static ERASE: fn() = safe;', ('static ERASE',), 'static', 'ERASE'),
                        ('let erase: fn() = safe;', (), 'let', 'erase')]
        for source, item, kind, binding in declarations:
            with self.subTest(source=source):
                self.assertTrue(gate.check_text(source, path))
                key = (path, item, kind, tuple(name for name, _ in gate.tokens(source)), binding)
                with patch.dict(gate.ALIAS_EXCEPTIONS, {key: 'fixture to isolate binding and use review'}):
                    self.assertEqual(gate.check_text(source, path), [])
                    use = binding + ('::from_raw_fd(fd)' if kind == 'type' else '()')
                    self.assertTrue(gate.check_text(source + ' fn probe() { ' + use + '; }', path))
        for source in ['type Native = ::std::os::fd::OwnedFd;',
                       'use ::std::os::fd::OwnedFd; type Native = OwnedFd;',
                       'type Next = Native; type Native = File;',
                       'type Native<T> = T;',
                       'type Native = <Hidden as Trait>::Associated;',
                       'type Native = Self;',
                       'const ERASE: fn() = std::fs::remove_file;',
                       'fn probe() { let erase = std::fs::remove_file::<_>; }']:
            with self.subTest(source=source):
                self.assertTrue(gate.check_text(source, path))

    def test_alias_binding_permissions_retain_literal_contents(self):
        path = gate.BOUNDARY
        source = 'const OP: fn() = loader("reviewed");'
        key = (path, ('const OP',), 'const', tuple(n for n, _ in gate.tokens(source, literals=True)), 'OP')
        with patch.dict(gate.ALIAS_EXCEPTIONS, {key: 'fixture for literal-bearing function binding'}):
            self.assertEqual(gate.check_text(source, path), [])
            self.assertTrue(gate.check_text(source.replace('reviewed', 'changed'), path))

    def test_alias_reexport_chains_and_uses_before_definitions_are_checked(self):
        path = gate.BOUNDARY
        source = 'pub(crate) use std::fs::remove_file as erase; pub(crate) use self::erase as wipe;'
        approved = {(path, (), ('std', 'fs', 'remove_file'), 'erase', True): 'fixture for first alias',
                    (path, (), ('self', 'erase'), 'wipe', True): 'fixture for chained alias'}
        with patch.dict(gate.IMPORT_EXCEPTIONS, approved):
            self.assertEqual(gate.check_text(source, path), [])
            self.assertTrue(gate.check_text('fn probe() { wipe(path); } ' + source, path))
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                commands = root / gate.COMMANDS
                (commands / 'journal').mkdir(parents=True)
                (root / path).write_text(source)
                (commands / 'ownership.rs').write_text('')
                (commands / 'journal.rs').write_text('fn probe() { filesystem::wipe(path); }')
                self.assertTrue(gate.check_tree(root))

    def test_macro_tt_substitution_cannot_hide_capability_tokens(self):
        source = ('macro_rules! r { ($($t:tt),*) => { $($t)* } } '
                  'fn probe(path: &Path) { r!(Path, ::, read_dir)(path); }')
        self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_split_macro_path_and_method_substitutions_are_rejected(self):
        for source in [
            'macro_rules! r { ($a:ident, $s:tt, $b:ident) => { $a $s $b } } '
            'r!(Path, ::, read_dir)(path);',
            'macro_rules! r { ($p:ident, $s:tt, $m:ident) => { $p $s $m() } } '
            'r!(path, ., read_dir);',
        ]:
            with self.subTest(source=source):
                self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_unlisted_macro_definition_is_rejected(self):
        self.assertTrue(gate.check_text(
            'macro_rules! unlisted { () => { 1 } }', gate.COMMANDS / 'journal.rs'))

    def test_macro_arguments_cannot_carry_unanchored_capability_tokens(self):
        self.assertTrue(gate.check_text(
            'format!("{}", read_dir);', gate.COMMANDS / 'journal.rs'))

    def test_macro_path_repetition_is_rejected(self):
        source = ('macro_rules! review_path { ($($p:ident),*) => { $($p)::* } } '
                  'review_path!(Path, read_dir)(path);')
        self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_macro_method_repetition_is_rejected(self):
        source = ('macro_rules! review_method { ($($m:ident),*) => { $($m()).* } } '
                  'review_method!(make_path, read_dir);')
        self.assertTrue(gate.check_text(source, gate.COMMANDS / 'journal.rs'))

    def test_macro_definition_allowlist_is_exact_and_independent_of_token_ban(self):
        path = gate.COMMANDS / 'journal.rs'
        safe = 'macro_rules! safe { ($($p:expr),*) => { [$($p),*] } } safe!(1, 2);'
        self.assertTrue(gate.check_text(safe, path))
        with patch.dict(gate.CRATE_MACRO_ALLOWLIST,
                        {(path, 'safe'): 'array construction cannot synthesize capability identifiers'}):
            self.assertEqual(gate.check_text(safe, path), [])
            self.assertTrue(gate.check_text(safe.replace('safe', 'other'), path))
            self.assertTrue(gate.check_text(safe, gate.COMMANDS / 'journal/compact.rs'))
            self.assertTrue(gate.check_text(safe.replace('safe!(1, 2)', 'safe!(read_dir)'), path))
            self.assertTrue(gate.check_text(safe.replace('[$($p),*]', 'read_dir'), path))
        for reason in ['', '   ']:
            with patch.dict(gate.CRATE_MACRO_ALLOWLIST, {(path, 'safe'): reason}):
                self.assertTrue(gate.check_text(safe, path))

    def test_macro_substitution_shape_cannot_bypass_capability_tokens(self):
        path = gate.COMMANDS / 'journal.rs'
        for body, invocation in [
            ('$($p)*', 'probe!(Path, ::, read_dir)(path)'),
            ('$a $s $b', 'probe!(Path, ::, read_dir)(path)'),
            ('$p $s $m()', 'probe!(path, ., read_dir)'),
            ('$($p)::*', 'probe!(Path, read_dir)(path)'),
            ('$($p()).*', 'probe!(make_path, read_dir)'),
            ('$(.$m())*', 'probe!(read_dir)'),
            ('$($($p)*)*', 'probe!(Path, ::, read_dir)(path)'),
        ]:
            with self.subTest(body=body):
                source = 'macro_rules! probe { ($($p:tt)*) => { ' + body + ' } } ' + invocation + ';'
                # A hypothetical definition approval cannot waive the token rule.
                with patch.dict(gate.CRATE_MACRO_ALLOWLIST,
                                {(path, 'probe'): 'fixture to isolate token checks from definition checks'}):
                    self.assertTrue(gate.capability_findings(source, gate.tokens(source), path))
                    self.assertTrue(gate.check_text(source, path))

    def test_unlisted_macros_fail_in_every_protected_file(self):
        for path in gate.protected_files(gate.ROOT):
            relative = path.relative_to(gate.ROOT)
            with self.subTest(path=relative):
                self.assertTrue(gate.check_text('macro_rules! added { () => { 1 } }', relative))

    def test_identifier_construction_crates_require_import_review(self):
        for source in ['use paste::paste; paste! { [<rea d_dir>](path); }',
                       'paste::paste! { [<rea d_dir>](path); }',
                       'use crate::other::paste; paste! { [<rea d_dir>](path); }']:
            with self.subTest(source=source):
                path = gate.COMMANDS / 'journal.rs'
                self.assertEqual(gate.capability_findings(source, gate.tokens(source), path), [])
                self.assertTrue(gate.check_text(source, path))

    def test_generated_and_nested_items_cannot_borrow_declaration_pins(self):
        path = gate.COMMANDS / 'journal.rs'
        for wrapper in ['macro_rules! probe { () => { %s } }',
                        'const NESTED: () = { %s };', 'const _: () = const { %s };',
                        'static mut NESTED: () = { %s };']:
            with self.subTest(wrapper=wrapper):
                self.assertTrue(gate.check_text(
                    wrapper % 'fn private() { file.sync_all(); }', path))
        self.assertTrue(gate.check_text(
            'impl Writer { fn open() { format!("{}", open); } }', path))
        self.assertTrue(gate.check_text(
            'impl Writer { fn spawn() { format!("{}", spawn); } }', path))

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

    def test_capability_tokens_need_anchors_in_every_protected_file(self):
        for path in gate.protected_files(gate.ROOT):
            relative = path.relative_to(gate.ROOT)
            for name in gate.CAPABILITY_METHODS:
                with self.subTest(path=relative, capability=name):
                    self.assertTrue(gate.check_text('format!("{}", ' + name + ');', relative))
        self.assertEqual(gate.check_text(
            '// object.read_dir();\nlet text = "object.read_dir()";',
            gate.COMMANDS / 'journal.rs'), [])
        self.assertTrue(gate.check_text('fn read_dir() {}', gate.COMMANDS / 'journal.rs'))
        boundary = (gate.ROOT / gate.BOUNDARY).read_text()
        changed = boundary.replace('::std::fs::File::open(path.as_ref())',
                                   '::std::fs::Other::open(path.as_ref())')
        self.assertNotEqual(boundary, changed)
        self.assertTrue(gate.check_text(changed, gate.BOUNDARY))

    def test_repository_uses_counted_boundary(self):
        self.assertEqual(gate.check_tree(gate.ROOT), [])


if __name__ == '__main__':
    unittest.main()
