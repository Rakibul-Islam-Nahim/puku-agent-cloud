-- Puku reliability rebuild: installed package inventory.
-- Per docs/RELIABILITY-REBUILD.md §3.5 and §4.5.
--
-- Filled by puku-guestd::package_watcher from inotify on package-manager
-- state directories (dpkg, pip dist-info, npm cache). Replayed by
-- puku-rebuild when an environment has to be reconstructed from scratch.

CREATE TABLE IF NOT EXISTS installed_packages (
    session_id  uuid NOT NULL,
    kind        text NOT NULL CHECK (kind IN ('apt','pip','npm','cargo','go','gem','brew','system')),
    name        text NOT NULL,
    version     text,
    ts          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, kind, name)
);

CREATE INDEX IF NOT EXISTS installed_packages_session_idx ON installed_packages (session_id);