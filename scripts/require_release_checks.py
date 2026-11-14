"""Require completed successful qualification workflows for this exact source commit."""
import json
import os
import subprocess


def qualified(runs, source, workflow):
    # Measured manual runs can narrow providers/payloads. Only automatic full
    # campaigns qualify distribution until input-specific receipts are checked.
    events = ('push', 'pull_request') if workflow == 'large-upload-qualification.yml' else ('push', 'pull_request', 'workflow_dispatch')
    return any(run['headSha'] == source and run['status'] == 'completed' and run['conclusion'] == 'success' and run['event'] in events for run in runs)


def main():
    source = os.environ['GITHUB_SHA']
    for workflow in ('ci.yml', 'large-upload-qualification.yml'):
        runs = json.loads(subprocess.check_output(['gh', 'run', 'list', '--workflow', workflow, '--commit', source, '--limit', '100', '--json', 'headSha,status,conclusion,event'], text=True))
        if not qualified(runs, source, workflow):
            raise SystemExit(f'Release blocked: no successful full {workflow} run for exact source {source}')
    print(f'Qualification passed for {source}')


if __name__ == '__main__': main()
