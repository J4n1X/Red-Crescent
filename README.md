# Red Crescent

A Rust-powered web server that renders dynamic HTML using embedded Lua scripts. Think PHP, but with Lua and Rust!
It's intended purpose is to be used behind another webserver such as Nginx or Apache2.

## Features

- **🌙 Lua Integration**: Embed Lua scripts directly in HTML using `<?lua ... ?>` tags
- **📋 Request API**: Access HTTP method, path, headers, and query parameters
- **⚙️ Response Control**: Set custom HTTP status codes and headers from Lua
- **📝 Server Logging**: Log messages from Lua using `log.info()`, `log.debug()`, etc.
- **😴 Performance?**: Built with Actix-web and mlua, so it shouldn't be that slow.

## Quick Start

1. **Clone and Build**:
   ```bash
   git clone <repository-url>
   cd lua-html-renderer
   cargo build --release
   ```

2. **Run the Server**:
   ```bash
   cargo run --release
   ```

3. **Access the Demos**:
   - Main Demo: `http://127.0.0.1:8080/demo.lhtml`
   - Request API Demo: `http://127.0.0.1:8080/request-demo.lhtml`
   - Lua Info: `http://127.0.0.1:8080/info.lhtml`

## Basic Usage

Create a `.lhtml` file in the `demos/` directory:

```html
<!DOCTYPE html>
<html>
<head>
    <title>Hello Lua!</title>
</head>
<body>
    <h1><?lua return "Hello, " .. (request.query.name or "World") .. "!" ?></h1>
    
    <?lua
        -- Multi-line Lua code
        local items = {"Apples", "Bananas", "Cherries"}
        print("<ul>")
        for _, item in ipairs(items) do
            print("<li>" .. item .. "</li>")
        end
        print("</ul>")
    ?>
</body>
</html>
```

## API Reference

### Request Object

Access HTTP request information via the global `request` table:

```lua
-- HTTP Method (GET, POST, PUT, DELETE, etc.)
local method = request.method

-- Request path
local path = request.path

-- Query parameters
local name = request.query.name
local age = request.query.age

-- HTTP headers
local user_agent = request.headers["user-agent"]
local content_type = request.headers["content-type"]
```

### Response Control

Control the HTTP response using the global `response` table:

```lua
-- Set status code (default is 200)
response.status = 404  -- Not Found
response.status = 301  -- Moved Permanently
response.status = 500  -- Internal Server Error

-- Set custom headers
response.headers["Cache-Control"] = "no-cache"
response.headers["X-Custom-Header"] = "Custom Value"
```

### Utility Functions

#### `print(...)`
Easy text output that concatenates arguments:

```lua
<?lua print("Hello, ", "World!") ?>
<?lua print("User: ", request.query.name) ?>
```

#### `html_escape(string)`
Escape HTML entities to prevent XSS attacks:

```lua
<?lua
    local user_input = request.query.comment or ""
    print("Safe output: " .. html_escape(user_input))
?>
```

This escapes: `<`, `>`, `&`, `"`, `'`

### Logging Functions

Log messages to the server console:

```lua
<?lua
    log.trace("Trace level message")
    log.debug("Debug information")
    log.info("Information message")
    log.warn("Warning message")
    log.error("Error message")
?>
```

## Advanced Examples

### Dynamic Status Codes

```lua
<?lua
    local page = request.query.page
    if not page then
        response.status = 404
        return "<h1>404 - Page Not Found</h1>"
    end
?>
```

### Custom Headers

```lua
<?lua
    response.headers["Content-Type"] = "application/json"
    response.headers["Access-Control-Allow-Origin"] = "*"
    return '{"status": "ok", "message": "Hello from Lua"}'
?>
```

### Safe User Input

```lua
<?lua
    local search = request.query.search or ""
    if search ~= "" then
        print("<p>Search results for: <strong>")
        print(html_escape(search))
        print("</strong></p>")
    end
?>
```

### Request Information Page

```lua
<h2>Request Details</h2>
<ul>
    <li>Method: <?lua print(request.method) ?></li>
    <li>Path: <?lua print(request.path) ?></li>
    <li>User-Agent: <?lua print(request.headers["user-agent"] or "N/A") ?></li>
</ul>

## Configuration

The server serves files from the `demos/` directory by default. Modify `src/config.rs` to change the served directory.

## Security Notes

- **XSS Prevention**: Always use `html_escape()` when outputting user input
- **Path Traversal**: The server automatically prevents directory traversal attacks
- **File Type Restriction**: Only `.lhtml` files are processed; other files return 400 Bad Request

## Performance

- Built with **Actix-web** for high-performance HTTP handling
- Uses **mlua** for efficient Lua integration
- Lua instances are reset between requests to maintain isolation

## Contributing

Contributions are welcome! Please open an issue or submit a pull request for any improvements or bug fixes.

## License

This project is licensed under the MIT License. See the LICENSE file for details.
