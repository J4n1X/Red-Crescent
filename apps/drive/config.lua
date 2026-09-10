-- Application settings for Drive. (Server-level limits live in rc_config.lua.)
return {
    -- Base URL for share links, no trailing slash. nil derives it per request,
    -- which yields whatever address you used -- possibly a LAN IP the recipient
    -- cannot reach. Set it once there is a stable public address.
    share_base_url = "https://drive.eicher.cc",

    -- How long a login stays valid.
    session_ttl = 7 * 24 * 3600,

    -- Failed-login throttling, in-app so it works with no proxy in front.
    -- Sliding window, counted per address and per username.
    login_window = 900,
    login_max_per_ip = 10,
    login_max_per_user = 20,

    -- Per-user storage in bytes; nil or 0 is unlimited. Overridable per account.
    default_quota_bytes = nil,

    -- Ceilings for building a folder archive.
    archive_max_files = 20000,
    archive_max_bytes = 8 * 1024 * 1024 * 1024,

    -- How long a built archive stays on disk before the cleanup thread
    -- removes it, and how often that thread runs (both in seconds).
    archive_retention = 600,
    cleanup_interval = 300,

    -- Age backstop for reusing a finished archive. Staleness is really handled
    -- by content -- an archive is reused only while the folder fingerprint
    -- matches -- so 0 disables the age check and trusts the signature alone.
    archive_reuse_secs = 600,

    -- How many exports may build at once; each is a zip process working
    -- through gigabytes. Jobs over the limit stay queued and start as slots
    -- free up.
    archive_max_concurrent = 2,
}
