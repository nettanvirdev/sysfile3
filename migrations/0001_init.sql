-- sysfile3 schema, version 1.
--
-- Every row belongs to one workspace, and row-level security keeps it there: each transaction
-- sets `sysfile3.workspace` to the workspace it works on (or `*` for maintenance), and the
-- policies hide every other workspace's rows, even from the table owner (FORCE).

CREATE FUNCTION sf3_current_workspace() RETURNS uuid
    LANGUAGE sql STABLE AS $$
    SELECT CASE WHEN s ~ '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
                THEN s::uuid END
    FROM (SELECT current_setting('sysfile3.workspace', true) AS s) AS setting
$$;

CREATE FUNCTION sf3_all_workspaces() RETURNS boolean
    LANGUAGE sql STABLE AS $$
    SELECT coalesce(current_setting('sysfile3.workspace', true) = '*', false)
$$;

-- One per (app, external id). The app owns users, plans and billing; sysfile3 keeps the limits
-- it is given and the usage it measures.
CREATE TABLE sf3_workspaces (
    id          uuid PRIMARY KEY,
    app         text NOT NULL,
    external_id text NOT NULL,
    quota_bytes bigint CHECK (quota_bytes >= 0),
    quota_nodes bigint CHECK (quota_nodes >= 0),
    used_bytes  bigint NOT NULL DEFAULT 0,
    node_count  bigint NOT NULL DEFAULT 0,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (app, external_id)
);

-- File contents. Each write stores a new object under a fresh id, so an object is never
-- rewritten and a cached copy can never go stale. Copies and restored versions point at an
-- existing blob instead of storing the bytes again.
CREATE TABLE sf3_blobs (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES sf3_workspaces (id) ON DELETE CASCADE,
    size         bigint NOT NULL CHECK (size >= 0),
    sha256       bytea,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX sf3_blobs_workspace ON sf3_blobs (workspace_id);

-- Files and folders. `path` is the whole normalized path ("/docs/a.pdf"), compared byte by
-- byte (COLLATE "C") so a folder's subtree is the range [path || '/', path || '0').
CREATE TABLE sf3_nodes (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES sf3_workspaces (id) ON DELETE CASCADE,
    path         text COLLATE "C" NOT NULL,
    depth        integer NOT NULL CHECK (depth >= 1),
    kind         text NOT NULL CHECK (kind IN ('file', 'dir')),
    blob_id      uuid REFERENCES sf3_blobs (id),
    size         bigint NOT NULL DEFAULT 0,
    mime         text,
    version      integer NOT NULL DEFAULT 0,
    created_at   timestamptz NOT NULL DEFAULT now(),
    created_by   text,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    trash_id     uuid,
    trashed_at   timestamptz,
    CHECK ((kind = 'file') = (blob_id IS NOT NULL))
);
CREATE UNIQUE INDEX sf3_nodes_live_path ON sf3_nodes (workspace_id, path) WHERE trash_id IS NULL;
CREATE INDEX sf3_nodes_trash ON sf3_nodes (workspace_id, trash_id) WHERE trash_id IS NOT NULL;
CREATE INDEX sf3_nodes_blob ON sf3_nodes (blob_id);

CREATE TABLE sf3_versions (
    node_id      uuid NOT NULL REFERENCES sf3_nodes (id) ON DELETE CASCADE,
    version      integer NOT NULL,
    workspace_id uuid NOT NULL REFERENCES sf3_workspaces (id) ON DELETE CASCADE,
    blob_id      uuid NOT NULL REFERENCES sf3_blobs (id),
    size         bigint NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    created_by   text,
    PRIMARY KEY (node_id, version)
);
CREATE INDEX sf3_versions_blob ON sf3_versions (blob_id);

-- One row per delete: the folder or file that was deleted, with everything under it sharing
-- its trash id in sf3_nodes.
CREATE TABLE sf3_trash (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES sf3_workspaces (id) ON DELETE CASCADE,
    path         text COLLATE "C" NOT NULL,
    kind         text NOT NULL,
    size         bigint NOT NULL,
    trashed_at   timestamptz NOT NULL DEFAULT now(),
    trashed_by   text
);
CREATE INDEX sf3_trash_age ON sf3_trash (trashed_at);
CREATE INDEX sf3_trash_workspace ON sf3_trash (workspace_id, trashed_at);

-- Read-only links. Only the SHA-256 of the token is stored, so a database leak leaks no links.
CREATE TABLE sf3_shares (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES sf3_workspaces (id) ON DELETE CASCADE,
    node_id      uuid NOT NULL REFERENCES sf3_nodes (id) ON DELETE CASCADE,
    token_hash   bytea NOT NULL UNIQUE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    created_by   text,
    expires_at   timestamptz,
    revoked_at   timestamptz
);
CREATE INDEX sf3_shares_node ON sf3_shares (workspace_id, node_id);

-- Uploads that go straight from a client to object storage through a signed URL.
CREATE TABLE sf3_uploads (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES sf3_workspaces (id) ON DELETE CASCADE,
    path         text COLLATE "C" NOT NULL,
    blob_id      uuid NOT NULL,
    size         bigint NOT NULL,
    mime         text,
    created_by   text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL
);
CREATE INDEX sf3_uploads_expiry ON sf3_uploads (expires_at);

ALTER TABLE sf3_workspaces ENABLE ROW LEVEL SECURITY;
ALTER TABLE sf3_workspaces FORCE ROW LEVEL SECURITY;
CREATE POLICY sf3_scope ON sf3_workspaces
    USING (sf3_all_workspaces() OR id = sf3_current_workspace())
    WITH CHECK (sf3_all_workspaces() OR id = sf3_current_workspace());

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['sf3_blobs', 'sf3_nodes', 'sf3_versions', 'sf3_trash', 'sf3_shares', 'sf3_uploads'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY sf3_scope ON %I
                 USING (sf3_all_workspaces() OR workspace_id = sf3_current_workspace())
                 WITH CHECK (sf3_all_workspaces() OR workspace_id = sf3_current_workspace())', t);
    END LOOP;
END $$;
