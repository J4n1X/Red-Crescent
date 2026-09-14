// Drive's client-side enhancements.
//
// Progressive: with JavaScript off every form and link below still works, it
// only loses the feedback. A CSP forbids inline handlers, so everything is
// delegated from here and the markup declares intent with data-* attributes.
(function () {
    "use strict";

    function el(tag, className, text) {
        var node = document.createElement(tag);
        if (className) { node.className = className; }
        if (text !== undefined) { node.textContent = text; }
        return node;
    }

    function humanSize(bytes) {
        if (!isFinite(bytes) || bytes < 0) { return "?"; }
        if (bytes < 1024) { return bytes + " B"; }
        var units = ["KiB", "MiB", "GiB", "TiB"];
        var value = bytes, unit = "B";
        for (var i = 0; i < units.length && value >= 1024; i++) {
            value /= 1024;
            unit = units[i];
        }
        return value.toFixed(1) + " " + unit;
    }

    function humanTime(seconds) {
        if (!isFinite(seconds) || seconds < 0) { return ""; }
        if (seconds < 60) { return Math.round(seconds) + "s"; }
        var minutes = Math.floor(seconds / 60);
        if (minutes < 60) { return minutes + "m " + Math.round(seconds % 60) + "s"; }
        return Math.floor(minutes / 60) + "h " + (minutes % 60) + "m";
    }

    // --- confirmation prompts for destructive actions -------------------
    // One delegated listener covers every form, including ones rendered later.
    document.addEventListener("submit", function (event) {
        var message = event.target.getAttribute && event.target.getAttribute("data-confirm");
        if (message && !window.confirm(message)) {
            event.preventDefault();
        }
    });

    // --- upload progress ------------------------------------------------
    //
    // XMLHttpRequest rather than fetch(), which has no upload progress event.
    // The server answers with a 302 back to the folder page; XHR follows it
    // transparently, so responseURL is where a normal form post would land.

    var inFlight = null;

    function warnBeforeLeaving(event) {
        event.preventDefault();
        event.returnValue = "";
        return "";
    }

    function totalBytes(files) {
        var total = 0;
        for (var i = 0; i < files.length; i++) { total += files[i].size; }
        return total;
    }

    function buildProgressPanel(form) {
        var panel = el("div", "uploading");

        var head = el("div", "uploading-head");
        head.appendChild(el("strong", "uploading-title", "Uploading…"));
        var cancel = el("button", "linklike uploading-cancel", "Cancel");
        cancel.type = "button";
        head.appendChild(cancel);
        panel.appendChild(head);

        var bar = document.createElement("progress");
        bar.className = "uploading-bar";
        bar.max = 100;
        bar.value = 0;
        panel.appendChild(bar);

        var stats = el("div", "uploading-stats meta");
        stats.setAttribute("role", "status");
        stats.setAttribute("aria-live", "polite");
        panel.appendChild(stats);

        form.parentNode.insertBefore(panel, form.nextSibling);
        return { panel: panel, bar: bar, stats: stats, cancel: cancel,
                 title: head.firstChild };
    }

    document.addEventListener("submit", function (event) {
        var form = event.target;
        if (!form.hasAttribute || !form.hasAttribute("data-upload")) { return; }
        // No XHR/FormData means no progress: let the browser post the form
        // the ordinary way rather than breaking the upload entirely.
        if (!window.XMLHttpRequest || !window.FormData) { return; }
        if (inFlight) { event.preventDefault(); return; }

        var input = form.querySelector('input[type="file"]');
        if (!input || !input.files || input.files.length === 0) { return; }

        event.preventDefault();

        var files = input.files;
        var expected = totalBytes(files);
        var ui = buildProgressPanel(form);
        var submit = form.querySelector('button[type="submit"], input[type="submit"]');
        if (submit) { submit.disabled = true; }

        var xhr = new XMLHttpRequest();
        inFlight = xhr;
        var started = Date.now();

        ui.cancel.addEventListener("click", function () {
            ui.title.textContent = "Cancelling…";
            xhr.abort();
        });

        xhr.upload.addEventListener("progress", function (e) {
            var loaded = e.loaded;
            var total = e.lengthComputable ? e.total : expected;
            var elapsed = (Date.now() - started) / 1000;
            var rate = elapsed > 0 ? loaded / elapsed : 0;

            if (total > 0) {
                ui.bar.value = Math.min(100, (loaded / total) * 100);
            } else {
                ui.bar.removeAttribute("value");
            }

            var parts = [humanSize(loaded) + " of " + humanSize(total)];
            if (rate > 0) { parts.push(humanSize(rate) + "/s"); }
            if (rate > 0 && total > loaded) {
                parts.push(humanTime((total - loaded) / rate) + " left");
            }
            ui.stats.textContent = parts.join(" · ");
        });

        // The bytes are up but the server is still writing them into SQLite;
        // for a few thousand files that pause is long enough to look stuck.
        xhr.upload.addEventListener("load", function () {
            ui.bar.removeAttribute("value");
            ui.title.textContent = "Saving on the server…";
            ui.stats.textContent = files.length + " file" +
                (files.length === 1 ? "" : "s") + " sent, finishing up";
        });

        xhr.addEventListener("load", function () {
            inFlight = null;
            window.removeEventListener("beforeunload", warnBeforeLeaving);
            if (xhr.status >= 200 && xhr.status < 400) {
                window.location = xhr.responseURL || window.location.href;
                return;
            }
            if (submit) { submit.disabled = false; }
            ui.panel.className = "uploading uploading-failed";
            ui.bar.value = 0;
            if (xhr.status === 413) {
                ui.title.textContent = "Too large to upload";
                ui.stats.textContent = "This upload is over the server's limit of " +
                    (form.getAttribute("data-max-size") || "the configured maximum") +
                    ". Send it in smaller batches.";
            } else {
                ui.title.textContent = "Upload failed";
                ui.stats.textContent = "The server answered " + xhr.status +
                    ". Nothing was stored — you can try again.";
            }
        });

        xhr.addEventListener("abort", function () {
            inFlight = null;
            window.removeEventListener("beforeunload", warnBeforeLeaving);
            if (submit) { submit.disabled = false; }
            ui.panel.parentNode.removeChild(ui.panel);
        });

        xhr.addEventListener("error", function () {
            inFlight = null;
            window.removeEventListener("beforeunload", warnBeforeLeaving);
            if (submit) { submit.disabled = false; }
            ui.panel.className = "uploading uploading-failed";
            ui.title.textContent = "Connection lost";
            ui.stats.textContent = "The upload did not reach the server. Nothing was stored.";
        });

        window.addEventListener("beforeunload", warnBeforeLeaving);
        xhr.open("POST", form.getAttribute("action") || window.location.href);
        xhr.send(new FormData(form));
    });

    // --- manage dialog ----------------------------------------------------
    //
    // Listing rows carry only a link: the forms live in one dialog per page, so
    // the folder <select> is emitted once rather than once per row. Without
    // script the link goes to manage.lhtml, which has the same forms.

    var manageDialog = document.getElementById("manage");

    document.addEventListener("click", function (event) {
        if (!manageDialog) { return; }

        if (event.target.classList && event.target.classList.contains("manage-close")) {
            manageDialog.close();
            return;
        }

        var link = event.target.closest ? event.target.closest("a[data-manage]") : null;
        if (!link || !manageDialog.showModal) { return; }
        if (event.button !== 0 || event.ctrlKey || event.metaKey ||
            event.shiftKey || event.altKey) { return; }
        event.preventDefault();

        // One dialog serves files and folders; the action names differ only by
        // suffix, so each form declares its operation and the kind completes it.
        var kind = link.getAttribute("data-kind") === "folder" ? "folder" : "file";
        var id = (link.href.match(/[?&](?:file|folder)=(\d+)/) || [])[1] || "";
        var forms = manageDialog.querySelectorAll("form[data-op]");
        for (var i = 0; i < forms.length; i++) {
            var op = forms[i].getAttribute("data-op");
            forms[i].querySelector('input[name="action"]').value = op + "_" + kind;
            forms[i].querySelector('input[name="id"]').value = id;
            if (op === "delete") {
                forms[i].setAttribute("data-confirm", kind === "folder"
                    ? "Delete this folder and everything inside it?"
                    : "Delete this file?");
            }
        }
        // Moving a folder into itself or its own subtree is refused by the
        // server; offering those options at all is just a trap. Paths are
        // unique, so a subtree is exactly the prefix match.
        var own = kind === "folder" ? (link.getAttribute("data-path") || "") : "";
        var select = manageDialog.querySelector('select[name="dest"]');
        if (select) {
            var options = select.options;
            for (var j = 0; j < options.length; j++) {
                var path = options[j].getAttribute("data-path") || "";
                var blocked = own !== "" &&
                    (path === own || path.indexOf(own + "/") === 0);
                options[j].hidden = blocked;
                options[j].disabled = blocked;
            }
            // The previous row's choice may now be hidden.
            select.value = "";
        }

        var name = link.getAttribute("data-name") || "";
        var field = manageDialog.querySelector('input[name="name"]');
        if (field) { field.value = name; }
        var title = manageDialog.querySelector(".manage-title");
        if (title) { title.textContent = name; }
        manageDialog.showModal();
    });

    // --- folder archives --------------------------------------------------
    //
    // The link means "enqueue a job". Without script it leads to a page that
    // refreshes until the archive is ready, so this only takes over on a plain
    // left click; with script a corner notice follows the job instead.

    var POLL_MS = 1000;
    var MAX_WAIT_MS = 20 * 60 * 1000;

    function noticeArea() {
        var area = document.querySelector(".archive-notices");
        if (!area) {
            area = el("div", "archive-notices");
            document.body.appendChild(area);
        }
        return area;
    }

    function makeNotice() {
        var notice = el("div", "archive-notice");
        notice.appendChild(el("span", "archive-spinner"));
        var text = el("div", "archive-notice-text");
        var title = el("strong", null, "Preparing your archive\u2026");
        var detail = el("div", "meta", "Queued for the export worker.");
        text.appendChild(title);
        text.appendChild(detail);
        notice.appendChild(text);
        noticeArea().appendChild(notice);
        return { notice: notice, title: title, detail: detail };
    }

    function askJson(url) {
        return fetch(url, { credentials: "same-origin" }).then(function (response) {
            if (!response.ok) { throw new Error("HTTP " + response.status); }
            return response.json();
        });
    }

    document.addEventListener("click", function (event) {
        var link = event.target.closest ? event.target.closest("a[data-archive]") : null;
        if (!link) { return; }
        // Modified clicks open a new tab or save directly; leave those to the
        // no-script path, which is self-contained.
        if (event.button !== 0 || event.ctrlKey || event.metaKey ||
            event.shiftKey || event.altKey) { return; }
        if (!window.fetch) { return; }

        event.preventDefault();

        var ui = makeNotice();
        var started = Date.now();

        function stop(className, title, detail) {
            ui.notice.className = "archive-notice " + className;
            ui.title.textContent = title;
            ui.detail.textContent = detail;
        }

        function poll(jobId) {
            if (Date.now() - started > MAX_WAIT_MS) {
                stop("archive-notice-stale", "Still preparing\u2026",
                    "This is taking unusually long. Reopen the folder download to check on it.");
                return;
            }
            askJson("/archive.lhtml?job=" + encodeURIComponent(jobId) + "&json=1")
                .then(function (data) {
                    if (data.status === "ready" && data.fetch) {
                        ui.title.textContent = "Starting download\u2026";
                        ui.detail.textContent = "";
                        // A Content-Disposition response does not unload the
                        // page, so the notice has to be cleared on a timer.
                        window.location = data.fetch;
                        window.setTimeout(function () {
                            if (ui.notice.parentNode) {
                                ui.notice.parentNode.removeChild(ui.notice);
                            }
                        }, 2000);
                        return;
                    }
                    if (data.status === "failed") {
                        stop("archive-notice-failed", "Archive failed",
                            data.error || "The archive could not be built.");
                        return;
                    }
                    if (data.status === "missing") {
                        stop("archive-notice-failed", "Archive expired",
                            "That archive is no longer available. Try the download again.");
                        return;
                    }
                    if (data.status === "building") {
                        ui.detail.textContent =
                            "Compressing\u2026 large folders can take a while.";
                    } else if (data.running > 0) {
                        // No position is claimed: starts go to whoever polls
                        // next, so a place in line would be a number we cannot
                        // honour.
                        var others = data.running === 1 ? "1 other export is"
                                                        : data.running + " other exports are";
                        var waiting = data.waiting > 0
                            ? " (" + data.waiting + " also waiting)" : "";
                        ui.detail.textContent =
                            "Queued \u2014 " + others + " running" + waiting +
                            ". Yours starts as soon as one finishes.";
                    } else {
                        ui.detail.textContent = "Queued \u2014 starting in a moment.";
                    }
                    window.setTimeout(function () { poll(jobId); }, POLL_MS);
                })
                .catch(function () {
                    stop("archive-notice-failed", "Lost contact",
                        "Could not reach the server. The archive may still be building.");
                });
        }

        var separator = link.href.indexOf("?") === -1 ? "?" : "&";
        askJson(link.href + separator + "json=1")
            .then(function (data) {
                if (!data.job) {
                    stop("archive-notice-failed", "Could not start",
                        data.error || "The archive could not be queued.");
                    return;
                }
                poll(data.job);
            })
            .catch(function () {
                stop("archive-notice-failed", "Could not start",
                    "Could not reach the server.");
            });
    });
}());
