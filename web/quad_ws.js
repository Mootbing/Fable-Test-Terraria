// quad_ws.js — minimal WebSocket plugin for miniquad/macroquad wasm builds.
// Binary frames only. One connection. Polling-based (drained from the game loop).
"use strict";
(function () {
    let socket = null;
    let status = 0;        // 0 = connecting, 1 = open, 2 = closed or error
    const queue = [];      // Array<Uint8Array> of received binary messages

    function register_plugin(importObject) {
        importObject.env.quad_ws_connect = function (url_ptr, url_len) {
            // Views into wasm memory are only valid during this synchronous call.
            const url = new TextDecoder().decode(
                new Uint8Array(wasm_memory.buffer, url_ptr, url_len));
            if (socket !== null) {
                // Detach the old handlers before closing: a previous
                // socket's async onclose firing later must not clobber
                // `status`/`socket` for the connection created below
                // (reconnect-while-still-connecting bug).
                socket.onopen = socket.onclose = socket.onerror = socket.onmessage = null;
                try { socket.close(); } catch (_) {}
            }
            status = 0;
            queue.length = 0;
            let s;
            try {
                s = new WebSocket(url);
            } catch (e) {           // e.g. SecurityError: ws:// from an https:// page
                console.error("quad_ws: connect failed", e);
                status = 2; socket = null; return;
            }
            socket = s;
            s.binaryType = "arraybuffer";   // ESSENTIAL: default is "blob"
            // Belt and braces with the detach above: every handler ignores
            // events unless `s` is still the current socket.
            s.onopen    = () => { if (socket === s) status = 1; };
            s.onclose   = () => { if (socket === s) { status = 2; socket = null; } };
            s.onerror   = (e) => { if (socket === s) console.error("quad_ws:", e); };
            s.onmessage = (ev) => {
                if (socket === s && ev.data instanceof ArrayBuffer) {
                    queue.push(new Uint8Array(ev.data));
                }
            };
        };

        importObject.env.quad_ws_status = function () { return status; };

        importObject.env.quad_ws_send = function (ptr, len) {
            if (socket === null || status !== 1) return;
            // .slice() copies out of wasm linear memory before handing to the socket.
            socket.send(new Uint8Array(wasm_memory.buffer, ptr, len).slice());
        };

        // Length of the next pending message, or -1 if none (0-length frames are legal).
        importObject.env.quad_ws_next_len = function () {
            return queue.length === 0 ? -1 : queue[0].length;
        };

        // Copy next message into (ptr, cap); returns bytes written, or -1 if none.
        importObject.env.quad_ws_recv = function (ptr, cap) {
            if (queue.length === 0) return -1;
            const msg = queue.shift();
            const n = Math.min(msg.length, cap);
            new Uint8Array(wasm_memory.buffer, ptr, n).set(msg.subarray(0, n));
            return n;
        };

        // Derive the game-server ws URL from the page location (wss on https
        // pages, same host, path /ws) and write it into (ptr, cap).
        // Returns bytes written, or -1 if it doesn't fit.
        importObject.env.quad_ws_default_url = function (ptr, cap) {
            const proto = location.protocol === "https:" ? "wss://" : "ws://";
            const bytes = new TextEncoder().encode(proto + location.host + "/ws");
            if (bytes.length > cap) return -1;
            new Uint8Array(wasm_memory.buffer, ptr, bytes.length).set(bytes);
            return bytes.length;
        };
    }

    miniquad_add_plugin({ register_plugin, version: 1, name: "quad_ws" });
})();
