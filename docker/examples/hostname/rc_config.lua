-- The image builds plugins into /opt/red-crescent/plugins, outside the serve
-- directory as the server requires.
return {
    plugins = {
        hostname = { path = "/opt/red-crescent/plugins/hostname" },
    },
}
