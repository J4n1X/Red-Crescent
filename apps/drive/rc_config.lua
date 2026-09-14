-- Red Crescent server settings for Drive.
--
-- Living in the serve directory means `--serve-dir apps/drive` is the only
-- flag needed. Flags and RC_* environment variables still override these.
return {
    bind = "127.0.0.1:8080",

    -- Database, stored files and upload spool. Must not be inside the serve
    -- directory; relative paths resolve from the working directory.
    data_dir = "./data",

    -- Bulk file storage: a real folder upload is gigabytes across thousands
    -- of files, so the stock limits are far too small.
    max_upload_size = 8 * 1024 * 1024 * 1024,
    max_upload_files = 20000,

    -- Archives build on a thread, not in the request, so this only covers
    -- ordinary work. Above the 5s default for the upload path, which stores a
    -- few thousand files in one transaction.
    timeout_ms = 10000,

    -- Threads have their own limits, so the jobs above are not bound by it.
    thread_memory_limit_mb = 64,
    
    -- Idle SQLite connections that can be reused. 
    -- That removes some overhead from opening connections.
    sqlite_idle_connections = 8,

    -- Housekeeping runs from boot rather than waiting for the first request.
    threads = { "jobs/cleanup.lua" },
}
