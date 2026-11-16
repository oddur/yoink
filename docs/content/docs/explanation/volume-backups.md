---
title: How volume backups work
description: The design of the restic backup service, why restic dedup makes off-site backups cheap, and how postgres WAL archiving closes the gap between snapshots.
---

The `restic-backups` template generates a yoink-managed service that runs [restic](https://restic.net/) on a schedule.

## The backup service

The `backups` service runs [`mazzolino/restic`](https://github.com/djmaze/resticker). This image has restic and go-cron in one container, so the service needs no `build:` block.

- The source docker volumes are mounted **read-only**. restic can read the volumes but cannot change them.
- `RESTIC_PASSWORD` encrypts the repo, and the S3 keys control access to it. They are sealed secrets and are never in cleartext on disk.
- The default retention is `--keep-daily 7 --keep-weekly 4 --keep-monthly 6`.

## Dedup

restic uses [content-defined chunking](https://restic.readthedocs.io/en/stable/100_references.html#design). It splits files into chunks of variable size at content boundaries. It hashes each chunk with SHA-256, encrypts it, and stores it by content address.

**A second backup sends only the chunks that the repo does not have.** If you edit one row in a 2 GB postgres dump, approximately 50 KB of new chunks go to the repo. Daily backups of data that changes slowly cost almost no bandwidth and storage. This property makes a self-hosted off-site backend over a slow link practical.

## PostgreSQL WAL archiving

A nightly restic snapshot of `pgdata-*` is sufficient for the data of yesterday. But a crash at 14:30, with the last snapshot at 03:30, loses 11 hours. The continuous WAL archiving of PostgreSQL closes that gap. PostgreSQL sends each write-ahead log segment to S3 when the segment is full. `restore_command` then replays the segments on any base backup.

With `wal_archiving=true`, the `postgres` template emits a service with these properties:

- It wraps the stock postgres entrypoint with a one-line `apk add aws-cli`.
- It starts postgres with `archive_mode=on`.
- It sets `wal_compression=zstd`, which compresses the full-page images in WAL records. The WAL on disk is thus denser before the upload.
- Its `archive_command` pipes each segment through `gzip -c` before the upload, so the segment files in S3 are `.gz` blobs. Segments of an idle database compress from 16 MiB to a few KiB, and busy segments compress to approximately 3-6 MiB.
- It sets `archive_timeout=300`, which forces a flush every 5 minutes. An idle database thus also sends its last segment.

yoink supplies only the upload half of point-in-time recovery. `archive_command` is the only contract that postgres has with the storage backend. The restore is a manual procedure outside the deploy reconcile loop.

The retention of the WAL archive is manual at this time.

## See also

- [Volume backups with restic](/docs/how-to/volume-backups): the procedures to configure backups, WAL archiving, and restores.
- [`yoink add` templates](/docs/explanation/templates): how the bundled templates generate services.
- [Sealed secrets](/docs/explanation/secrets): how yoink stores `RESTIC_PASSWORD` and the S3 keys.
- [Config reference](/docs/reference/config): the service fields that the generated `backups` service uses.
- [PostgreSQL continuous archiving docs](https://www.postgresql.org/docs/current/continuous-archiving.html): the full mechanics of point-in-time recovery.
