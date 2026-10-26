"""Rebuild and compare the client dependency FOD (ADR-0038 §6, #1838)."""
import argparse
import json
from pathlib import Path
import subprocess
import sys

SYSTEMS = {'x86_64-linux', 'aarch64-darwin'}
RECOVERY = (
    'For a dependency hash mismatch, run `just client-deps-hash` and commit '
    'nix/client-deps.json. A same-version nixpkgs Bun patch can reuse the cached '
    'fixed-output outPath; refresh explicitly after such a patch.'
)


def run(arguments):
    return subprocess.check_output(arguments, text=True).strip()


def build_report(system, destination, pin_path=Path('nix/client-deps.json')):
    if system not in SYSTEMS:
        raise ValueError(f'unsupported client dependency check system: {system}')
    target = f'.#packages.{system}.flock-client.deps'
    if run(['nix', 'eval', '--raw', target + '.system']) != system:
        raise ValueError('client dependency derivation system does not match its report')
    output = run(['nix', 'build', target, '--no-link', '--print-out-paths'])
    if '\n' in output or not Path(output).is_dir():
        raise ValueError('client dependency build did not return one output directory')
    # A cached FOD alone cannot prove that this system reproduces its contents.
    run(['nix', 'build', target, '--no-link', '--rebuild'])
    actual = run(['nix', 'hash', 'path', '--type', 'sha256', '--sri', output])
    pin = json.loads(pin_path.read_text())
    if actual != pin['hash']:
        raise ValueError(f'{system} produced {actual}, expected {pin["hash"]}')
    report = dict(system=system, hash=actual, bun=pin['bun'], lock=pin['lock'])
    destination.write_text(json.dumps(report, indent=2) + '\n')


def compare_reports(paths):
    reports = [json.loads(path.read_text()) for path in paths]
    if len(reports) != len(SYSTEMS) or {row['system'] for row in reports} != SYSTEMS:
        raise ValueError('need exactly one report each for x86_64-linux and aarch64-darwin')
    for key in ['hash', 'lock', 'bun']:
        if any(not isinstance(row[key], str) or not row[key] for row in reports):
            raise ValueError(f'client dependency reports require a nonempty {key}')
        if len({row[key] for row in reports}) != 1:
            values = ', '.join(f'{row["system"]}={row[key]}' for row in reports)
            raise ValueError(f'client dependency {key} differs across systems: {values}')
    return reports[0]['hash']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    build = commands.add_parser('build')
    build.add_argument('system', choices=sorted(SYSTEMS))
    build.add_argument('report', type=Path)
    compare = commands.add_parser('compare')
    compare.add_argument('reports', type=Path, nargs='+')
    args = parser.parse_args()
    try:
        if args.command == 'build':
            build_report(args.system, args.report)
        else:
            print('Client dependency FOD hashes match: ' + compare_reports(args.reports))
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f'client-deps: {error}\n{RECOVERY}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
