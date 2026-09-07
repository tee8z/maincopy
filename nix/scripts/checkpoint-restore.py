#!/usr/bin/env python3
"""Download a complete encrypted checkpoint, verify it, replay LTX, and accept it offline."""
import argparse
import json
import os
from pathlib import Path
import re
import tempfile

from backup_common import (BackupFailure, add_common_arguments,
                           manifest_inventory, protected_directory, protected_file, rclone, run,
                           runtime_config)
from backup_epochs import epoch_time, selection


def restore(args):
    directory = Path(args.directory)
    # Never merge an untrusted download into existing data or accept an
    # independently supplied database as if it had been replayed from the LTX.
    if directory.exists():
        raise BackupFailure("restore directory must not already exist")
    protected_directory(directory)
    with tempfile.TemporaryDirectory(prefix=".credentials-", dir=directory) as temporary:
        staging = Path(temporary)
        cache = staging / "unused-local-cache"
        protected_directory(cache)
        config = runtime_config(args, staging, cache)
        if (args.epoch is None) != (args.checkpoint is None):
            raise BackupFailure("explicit recovery requires both epoch and checkpoint")
        if args.checkpoint is None:
            selected = staging / "selection.json"
            rclone(args, config, "copyto", "--max-transfer", "1024", "--cutoff-mode", "hard",
                   "selectioncrypt:latest.json", str(selected))
            epoch, checkpoint = selection(json.loads(protected_file(selected, 1024)))
        else:
            epoch, checkpoint = args.epoch, args.checkpoint
            epoch_time(epoch)
            epoch_time(checkpoint)
        epoch_config = staging / "epoch-config"
        protected_directory(epoch_config)
        config = runtime_config(args, epoch_config, cache, epoch)
        source = "checkpoints/" + checkpoint + ".json"
        manifest_path = directory / "checkpoint.json"
        rclone(args, config, "copyto", "--max-transfer", "4194304", "--cutoff-mode", "hard",
               "offsitecrypt:" + source, str(manifest_path))
        _, inventory = manifest_inventory(manifest_path)
        for relative, identity in inventory:
            output = directory / relative
            output.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            rclone(args, config, "copyto", "--max-transfer", str(identity["bytes"]), "--cutoff-mode", "hard",
                   "offsitecrypt:objects/" + identity["digest"], str(output))
            if output.stat().st_size != identity["bytes"]:
                raise BackupFailure("checkpoint download size validation")
        protected_directory(directory / "content-candidates")
        validation_arguments = ["--manifest-file", str(manifest_path),
                                "--ltx-root", str(directory / "ltx"),
                                "--artifact-root", str(directory / "content-candidates")]
        result = json.loads(run([args.maincopyd, "--config", args.config, "verify-checkpoint",
                                 *validation_arguments], "checkpoint verification", 180, maximum=1024))
        txid = result.get("max_txid")
        if set(result) != {"max_txid"} or not isinstance(txid, str) or not re.fullmatch(r"[0-9a-f]{16}", txid):
            raise BackupFailure("verified recovery cutoff validation")
        database = directory / "database.sqlite3"
        # The SQLite file consumed by restore-replica is exactly the output of
        # this pinned native replay of the previously verified LTX inventory.
        run([args.litestream, "restore", "-txid", txid, "-integrity-check", "full",
             "-o", str(database), "file://" + str(directory)], "native Litestream replay", 600)
        run([args.maincopyd, "--config", args.config, "restore-replica",
             "--database-file", str(database), *validation_arguments], "offline restore acceptance", 600)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    add_common_arguments(parser)
    parser.add_argument("--directory", required=True, help="New protected recovery workspace; must not exist")
    parser.add_argument("--checkpoint", help="Explicit UTC-UUID checkpoint name; default is the last completely published checkpoint")
    parser.add_argument("--epoch", help="UTC-UUID epoch containing the explicitly selected checkpoint")
    args = parser.parse_args()
    os.umask(0o077)
    try:
        restore(args)
    except (OSError, ValueError, KeyError, TypeError, AttributeError, BackupFailure) as error:
        stage = str(error) if isinstance(error, BackupFailure) else "protected checkpoint recovery"
        parser.exit(1, f"Maincopy checkpoint restore failed during {stage}.\n")
    print("Complete checkpoint replayed, verified, and accepted for first start.")


if __name__ == "__main__":
    main()
