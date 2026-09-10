-- Database access and schema migrations for Drive.
local M = {}

M.FILES_DIR = server.data_dir .. "/drive/files"

-- Each entry is one migration: a list of single SQL statements.
-- Statements are idempotent (IF NOT EXISTS) so concurrent first requests
-- can't trip over each other.
local MIGRATIONS = {
    {
        [[CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY,
            username TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            is_admin INTEGER NOT NULL DEFAULT 0,
            approved INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        )]],
        [[CREATE TABLE IF NOT EXISTS sessions (
            token_hash TEXT PRIMARY KEY,
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            csrf_token TEXT NOT NULL,
            expires_at INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        )]],
        [[CREATE TABLE IF NOT EXISTS folders (
            id INTEGER PRIMARY KEY,
            owner_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            parent_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE(owner_id, parent_id, name)
        )]],
        [[CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            owner_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            folder_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            stored_name TEXT NOT NULL UNIQUE,
            size INTEGER NOT NULL,
            content_type TEXT,
            created_at INTEGER NOT NULL,
            UNIQUE(owner_id, folder_id, name)
        )]],
        [[CREATE TABLE IF NOT EXISTS shares (
            token TEXT PRIMARY KEY,
            file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            expires_at INTEGER,
            downloads INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        )]],
        "CREATE INDEX IF NOT EXISTS idx_files_owner_folder ON files(owner_id, folder_id)",
        "CREATE INDEX IF NOT EXISTS idx_folders_owner_parent ON folders(owner_id, parent_id)",
        "CREATE INDEX IF NOT EXISTS idx_sessions_expiry ON sessions(expires_at)",
        "CREATE INDEX IF NOT EXISTS idx_shares_file ON shares(file_id)",
    },
    -- 2: shares can point at a folder instead of a file. SQLite cannot relax
    -- the NOT NULL on file_id in place, so the table is rebuilt.
    {
        [[CREATE TABLE shares_v2 (
            token TEXT PRIMARY KEY,
            file_id INTEGER REFERENCES files(id) ON DELETE CASCADE,
            folder_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
            created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            expires_at INTEGER,
            downloads INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            CHECK ((file_id IS NULL) <> (folder_id IS NULL))
        )]],
        [[INSERT INTO shares_v2 (token, file_id, created_by, expires_at, downloads, created_at)
          SELECT token, file_id, created_by, expires_at, downloads, created_at FROM shares]],
        "DROP TABLE shares",
        "ALTER TABLE shares_v2 RENAME TO shares",
        "CREATE INDEX IF NOT EXISTS idx_shares_file ON shares(file_id)",
        "CREATE INDEX IF NOT EXISTS idx_shares_folder ON shares(folder_id)",
    },
    -- 3: per-user storage quotas, and failed-login throttling that does not
    -- depend on having a reverse proxy in front.
    {
        "ALTER TABLE users ADD COLUMN quota_bytes INTEGER",
        [[CREATE TABLE IF NOT EXISTS login_attempts (
            id INTEGER PRIMARY KEY,
            ip TEXT NOT NULL,
            username TEXT NOT NULL,
            at INTEGER NOT NULL
        )]],
        "CREATE INDEX IF NOT EXISTS idx_login_attempts_at ON login_attempts(at)",
        "CREATE INDEX IF NOT EXISTS idx_login_attempts_ip ON login_attempts(ip, at)",
        "CREATE INDEX IF NOT EXISTS idx_login_attempts_user ON login_attempts(username, at)",
    },
    -- 4: folder archives are built by a worker thread instead of inside the
    -- request, so a multi-gigabyte export no longer holds a connection open
    -- (or needs a request timeout sized for it). The queue lives here rather
    -- than in the thread registry because it has to survive a restart.
    {
        [[CREATE TABLE IF NOT EXISTS archive_jobs (
            id INTEGER PRIMARY KEY,
            owner_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            folder_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
            status TEXT NOT NULL,
            path TEXT,
            filename TEXT,
            content_type TEXT,
            error TEXT,
            created_at INTEGER NOT NULL,
            finished_at INTEGER
        )]],
        "CREATE INDEX IF NOT EXISTS idx_archive_jobs_queue ON archive_jobs(status, id)",
        [[CREATE INDEX IF NOT EXISTS idx_archive_jobs_owner
          ON archive_jobs(owner_id, folder_id, status)]],
    },
    -- 5: archives are reused only while the tree they were built from is
    -- unchanged, which needs the fingerprint stored alongside the job.
    {
        "ALTER TABLE archive_jobs ADD COLUMN signature TEXT",
        [[CREATE INDEX IF NOT EXISTS idx_archive_jobs_signature
          ON archive_jobs(owner_id, folder_id, signature, status)]],
    },
}

local function shell_quote(path)
    return "'" .. path:gsub("'", "'\\''") .. "'"
end

function M.open()
    local db = sqlite.open("drive/drive.db")
    db:execute("PRAGMA foreign_keys = ON")

    db:execute("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")
    local row = db:query("SELECT version FROM schema_version")[1]
    local version = row and row.version or 0
    for v = version + 1, #MIGRATIONS do
        for _, statement in ipairs(MIGRATIONS[v]) do
            db:execute(statement)
        end
        db:execute("DELETE FROM schema_version")
        db:execute("INSERT INTO schema_version (version) VALUES (?)", { v })
        log.info("drive: applied schema migration " .. v)
    end

    os.execute("mkdir -p " .. shell_quote(M.FILES_DIR))

    -- Make sure the housekeeping thread is alive. Idempotent by name, and it
    -- resurrects the thread if it ever died — no --thread flag required.
    thread.spawn("drive-cleanup", "jobs/cleanup.lua")

    return db
end

return M
