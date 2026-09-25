/* The Red Crescent plugin ABI. See ABI.md for the rules behind each entry.
 *
 * sdk/src/ffi.rs mirrors this file for Rust; keep the two in step. */

#ifndef RC_PLUGIN_H
#define RC_PLUGIN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#  define RC_EXPORT __declspec(dllexport)
#else
#  define RC_EXPORT __attribute__((visibility("default")))
#endif

#define RC_PLUGIN_ABI 1

typedef enum {
    RC_OK        = 0,
    RC_ERROR     = 1,   /* the plugin failed; the message was set with host->error */
    RC_PROPAGATE = 2    /* a host call failed; return this unchanged */
} RcStatus;

typedef enum {
    RC_NIL, RC_BOOL, RC_INT, RC_NUMBER, RC_STRING,
    RC_TABLE,           /* reference */
    RC_FUNCTION,        /* reference */
    RC_HANDLE,          /* an object of one of this plugin's handle types */
    RC_OTHER            /* reference to anything else: a coroutine, a foreign userdata */
} RcType;

/* A Lua table, function or other value, owned by Lua. Only usable through the host. */
typedef struct { uint64_t id; } RcRef;

typedef struct {
    RcType type;
    union {
        int b;
        int64_t i;
        double n;
        struct { const char *ptr; size_t len; } s;      /* binary-safe, borrowed */
        RcRef ref;
        struct { uint32_t type_id; void *ptr; } handle;
    } as;
} RcValue;

typedef struct RcHost RcHost;

typedef struct RcCall {
    const RcHost *host;
    const RcValue *args;
    size_t nargs;
    void *internal;     /* the host's; do not touch */
} RcCall;

/* Every function and method a plugin registers has this shape. */
typedef RcStatus (*RcFunction)(RcCall *call, void *userdata);

typedef struct {
    uint32_t size;
    uint32_t abi;                   /* the host's RC_PLUGIN_ABI */
    const char *server_version_cstr;
    const char *name_cstr;          /* the plugin's key in rc_config.lua: its Lua table name */
    const char *serve_dir_cstr;
    const char *data_dir_cstr;
    const char *options_json_cstr;  /* the plugin's `options` from rc_config.lua, as JSON, or "{}" */
    const RcHost *host;             /* valid for the life of the process */
} RcPluginContext;

enum {
    RC_FN_BLOCKING = 1 << 0     /* may wait (network, disk): keeps the template off the inline path */
};

typedef struct {
    const char *name_cstr;
    RcFunction fn;
    uint32_t flags;
} RcMethod;

typedef struct {
    const char *name_cstr;      /* shown by tostring() and in errors */
    const RcMethod *methods;    /* obj:method(...) passes the object as args[0] */
    size_t nmethods;
    void (*destroy)(void *ptr); /* runs when Lua collects the object; must not call the host */
    void *userdata;             /* handed to every method */
} RcHandleType;

typedef struct RcRegistryCOM RcRegistryCOM;
struct RcRegistryCOM {
    uint32_t size;
    /* name.fn: installs `fn` as a field of the plugin's table. */
    RcStatus (*function)(RcRegistryCOM *self, const char *name_cstr,
                         RcFunction fn, void *userdata, uint32_t flags);
    /* Declares a handle type; *type_id is what push_handle takes. */
    RcStatus (*handle_type)(RcRegistryCOM *self, const RcHandleType *type, uint32_t *type_id);
};

struct RcHost {
    uint32_t size;

    /* Results and errors */
    RcStatus (*push)(RcCall *call, const RcValue *value);               /* append a return value */
    RcStatus (*push_handle)(RcCall *call, uint32_t type_id, void *ptr); /* Lua now owns ptr */
    RcStatus (*error)(RcCall *call, const char *msg, size_t len);       /* then return RC_ERROR */

    /* Output, into the page being rendered; unescaped, like print() */
    RcStatus (*write)(RcCall *call, const char *bytes, size_t len);

    /* Functions */
    RcStatus (*call)(RcCall *call, RcRef fn, const RcValue *args, size_t nargs,
                     RcValue *results, size_t max_results, size_t *nresults);

    /* Tables */
    RcStatus (*new_table)(RcCall *call, RcRef *out);
    RcStatus (*table_get)(RcCall *call, RcRef table, const RcValue *key, RcValue *out);
    RcStatus (*table_set)(RcCall *call, RcRef table, const RcValue *key, const RcValue *value);
    /* Start with key = nil; a nil key back means the end. */
    RcStatus (*table_next)(RcCall *call, RcRef table, RcValue *key, RcValue *value);
    RcStatus (*table_len)(RcCall *call, RcRef table, size_t *out);

    /* Lifetime */
    RcStatus (*retain)(RcCall *call, RcRef ref);    /* keep for the rest of the request */
    RcStatus (*release)(RcCall *call, RcRef ref);

    /* Logging, same sink as log.info() and friends; level 0-3 = debug..error */
    void (*log)(int level, const char *msg, size_t len);
};

/* The two exports every plugin provides. */
RC_EXPORT uint32_t rc_plugin_abi(void);
RC_EXPORT RcStatus rc_plugin_install(const RcPluginContext *ctx, RcRegistryCOM *reg);

#ifdef __cplusplus
}
#endif

#endif /* RC_PLUGIN_H */
