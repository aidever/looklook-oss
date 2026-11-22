/**
 * 一条终端连接在浏览器里的状态：屏幕、还没被确认的输入。
 */
export class TermClient {
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        TermClientFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_termclient_free(ptr, 0);
    }
    /**
     * 当前帧画完了
     * @returns {Uint8Array}
     */
    ack() {
        const ret = wasm.termclient_ack(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * @returns {number}
     */
    acked_input_seq() {
        const ret = wasm.termclient_acked_input_seq(this.__wbg_ptr);
        return ret;
    }
    /**
     * @returns {number}
     */
    cols() {
        const ret = wasm.termclient_cols(this.__wbg_ptr);
        return ret;
    }
    /**
     * 告诉 termd 这个端支持哪些功能（位见 `term_proto::FEATURE_*`；1 = trzsz）。只在 welcome 的 `caps` 里有对应能力时发
     * @param {number} flags
     * @returns {Uint8Array}
     */
    features(flags) {
        const ret = wasm.termclient_features(this.__wbg_ptr, flags);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * 实时通道解压后的字节。返回一个 JSON 数组（每项一个更新，见 `push_*`）。
     * @param {Uint8Array | number[]} plain
     * @returns {string}
     */
    feed(plain) {
        let deferred2_0;
        let deferred2_1;
        try {
            const ptr0 = passArray8ToWasm0(plain, wasm.__wbindgen_malloc);
            const len0 = WASM_VECTOR_LEN;
            const ret = wasm.termclient_feed(this.__wbg_ptr, ptr0, len0);
            deferred2_0 = ret[0];
            deferred2_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred2_0, deferred2_1, 1);
        }
    }
    /**
     * 回滚通道（`/i/{id}/term-history`）解压后的字节。
     * @param {Uint8Array | number[]} plain
     * @returns {string}
     */
    feed_history(plain) {
        let deferred2_0;
        let deferred2_1;
        try {
            const ptr0 = passArray8ToWasm0(plain, wasm.__wbindgen_malloc);
            const len0 = WASM_VECTOR_LEN;
            const ret = wasm.termclient_feed_history(this.__wbg_ptr, ptr0, len0);
            deferred2_0 = ret[0];
            deferred2_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred2_0, deferred2_1, 1);
        }
    }
    /**
     * @returns {number}
     */
    frame() {
        const ret = wasm.termclient_frame(this.__wbg_ptr);
        return ret;
    }
    /**
     * 连接上的第一条消息。
     * @param {number} cols
     * @param {number} rows
     * @param {string | null | undefined} client_id
     * @param {boolean} resume
     * @returns {Uint8Array}
     */
    hello(cols, rows, client_id, resume) {
        const val2 = +cols;
        const val3 = +rows;
        const val4 = +resume;
        var ptr0 = isLikeNone(client_id) ? 0 : passStringToWasm0(client_id, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        var len0 = WASM_VECTOR_LEN;
        const ret = wasm.termclient_hello(this.__wbg_ptr, val2, val3, ptr0, len0, val4);
        var v5 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v5;
    }
    /**
     * 本地最早的一行、termd 还留着的最早一行、屏幕第 0 行的行号：`[本地最早, termd 最早, 屏幕起点]`
     * @returns {Float64Array}
     */
    history_bounds() {
        const ret = wasm.termclient_history_bounds(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * [from, to) 里第一段本地没有、termd 还留着的回滚行：`[epoch, 起点, 行数]`，都有了返回空数组
     * @param {number} from
     * @param {number} to
     * @returns {Float64Array}
     */
    history_missing(from, to) {
        const ret = wasm.termclient_history_missing(this.__wbg_ptr, from, to);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * JS 的数是 f64（行号、请求号远小于 2^53）；wasm-bindgen 的 u64 参数要 BigInt，用起来容易错，这里收 f64
     * @param {number} request_id
     * @param {number} epoch
     * @param {number} start_seq
     * @param {number} count
     * @returns {Uint8Array}
     */
    history_req(request_id, epoch, start_seq, count) {
        const ret = wasm.termclient_history_req(this.__wbg_ptr, request_id, epoch, start_seq, count);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * 按键 / 粘贴：分配输入序号，留一份直到 termd 确认处理过（断线续上后重发，termd 按序号去重）。
     * @param {Uint8Array | number[]} data
     * @returns {Uint8Array}
     */
    input(data) {
        const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.termclient_input(this.__wbg_ptr, ptr0, len0);
        var v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v2;
    }
    /**
     * 键盘打的字（可以本地预测回显，§5.7）；粘贴、编好的转义走 `input`
     * @param {Uint8Array | number[]} data
     * @returns {Uint8Array}
     */
    input_typed(data) {
        const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.termclient_input_typed(this.__wbg_ptr, ptr0, len0);
        var v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v2;
    }
    /**
     * 回滚行留在 WASM 里（电脑 wgpu 渲染）。之后 `scrolled` / `history` 更新里不再带行内容，只有 `n`（行数）
     */
    keep_history() {
        wasm.termclient_keep_history(this.__wbg_ptr);
    }
    /**
     * 已发出的最后一个输入序号、termd 确认到的序号（JS 量往返时间）
     * @returns {number}
     */
    last_input_seq() {
        const ret = wasm.termclient_last_input_seq(this.__wbg_ptr);
        return ret;
    }
    /**
     * 鼠标：`kind` 0 按下 1 抬起 2 拖动 3 移动 4 上滚 5 下滚；行列从 0 起。termd 按程序当前的鼠标模式编码。
     * @param {number} kind
     * @param {number} button
     * @param {number} col
     * @param {number} row
     * @param {number} modifiers
     * @returns {Uint8Array}
     */
    mouse(kind, button, col, row, modifiers) {
        const ret = wasm.termclient_mouse(this.__wbg_ptr, kind, button, col, row, modifiers);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    constructor() {
        const ret = wasm.termclient_new();
        this.__wbg_ptr = ret;
        TermClientFinalization.register(this, this.__wbg_ptr, this);
        return this;
    }
    /**
     * @param {number} i
     * @returns {Uint8Array}
     */
    pending_input(i) {
        const ret = wasm.termclient_pending_input(this.__wbg_ptr, i);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * 还没被确认的输入条数（续接后逐条 `pending_input(i)` 重发）
     * @returns {number}
     */
    pending_inputs() {
        const ret = wasm.termclient_pending_inputs(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * 有预测时光标该在的位置 `[行, 列]`，没有预测为空
     * @returns {Uint16Array}
     */
    predict_cursor() {
        const ret = wasm.termclient_predict_cursor(this.__wbg_ptr);
        var v1 = getArrayU16FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 2, 2);
        return v1;
    }
    /**
     * 预测变了就加一（重画用）
     * @returns {number}
     */
    predict_version() {
        const ret = wasm.termclient_predict_version(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * @returns {boolean}
     */
    predicting() {
        const ret = wasm.termclient_predicting(this.__wbg_ptr);
        return ret !== 0;
    }
    /**
     * 还没确认的预测：每个 `[行, 列, 字符码]` 摊平
     * @returns {Uint32Array}
     */
    predictions() {
        const ret = wasm.termclient_predictions(this.__wbg_ptr);
        var v1 = getArrayU32FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
        return v1;
    }
    /**
     * trzsz 原始字节模式的进度（见 `ClientMsg::Raw`）
     * @param {number} id
     * @param {number} consumed
     * @param {Uint8Array | number[]} write
     * @param {boolean} done
     * @returns {Uint8Array}
     */
    raw_progress(id, consumed, write, done) {
        const val2 = +id;
        const val3 = +consumed;
        const val4 = +done;
        const ptr0 = passArray8ToWasm0(write, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.termclient_raw_progress(this.__wbg_ptr, val2, val3, ptr0, len0, val4);
        var v5 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v5;
    }
    /**
     * 回滚通道重新连上（它有自己的解压上下文和拆包状态）
     */
    reset_history() {
        wasm.termclient_reset_history(this.__wbg_ptr);
    }
    /**
     * 新连接开始（解压上下文已在 JS 端重建）。`keep_screen`：续接时保留屏幕，Hello 带上帧号，termd 能接着发差量。
     * @param {boolean} keep_screen
     */
    reset_stream(keep_screen) {
        wasm.termclient_reset_stream(this.__wbg_ptr, keep_screen);
    }
    /**
     * @param {number} cols
     * @param {number} rows
     * @returns {Uint8Array}
     */
    resize(cols, rows) {
        const ret = wasm.termclient_resize(this.__wbg_ptr, cols, rows);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * 屏幕第 `i` 行的段（JSON 数组）
     * @param {number} i
     * @returns {string}
     */
    row(i) {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.termclient_row(this.__wbg_ptr, i);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * 屏幕第 `i` 行的纯文字（行尾空白去掉），给复制、`window.term.buffer` 兼容层用
     * @param {number} i
     * @returns {string}
     */
    row_text(i) {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.termclient_row_text(this.__wbg_ptr, i);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * 第 `i` 行是不是上一行自动折过来的（这一行的 `wrapped` 表示它折到了下一行）
     * @param {number} i
     * @returns {boolean}
     */
    row_wrapped(i) {
        const ret = wasm.termclient_row_wrapped(this.__wbg_ptr, i);
        return ret !== 0;
    }
    /**
     * @returns {number}
     */
    rows() {
        const ret = wasm.termclient_rows(this.__wbg_ptr);
        return ret;
    }
    /**
     * 往返够慢时打开本地预测（JS 量往返时间）
     * @param {boolean} on
     */
    set_predict(on) {
        wasm.termclient_set_predict(this.__wbg_ptr, on);
    }
    /**
     * 按顺序取出 `{"t":"raw"}` 更新对应的字节
     * @returns {Uint8Array}
     */
    take_raw() {
        const ret = wasm.termclient_take_raw(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * 0 = 前台，1 = 后台（每秒 1 帧），2 = 看不见（停发）
     * @param {number} state
     * @returns {Uint8Array}
     */
    view(state) {
        const ret = wasm.termclient_view(this.__wbg_ptr, state);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
}
if (Symbol.dispose) TermClient.prototype[Symbol.dispose] = TermClient.prototype.free;
function __wbg_get_imports() {
    const import0 = {
        __proto__: null,
        __wbg___wbindgen_throw_4c88d8bf264fd984: function(arg0, arg1) {
            throw new Error(getStringFromWasm0(arg0, arg1));
        },
        __wbindgen_init_externref_table: function() {
            const table = wasm.__wbindgen_externrefs;
            table.set(0, undefined);
            table.set(1024, undefined);
            table.set(1025, null);
            table.set(1026, true);
            table.set(1027, false);
        },
    };
    return {
        __proto__: null,
        "./term_web_bg.js": import0,
    };
}

const TermClientFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_termclient_free(ptr, 1));

function getArrayF64FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getFloat64ArrayMemory0().subarray(ptr / 8, ptr / 8 + len);
}

function getArrayU16FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint16ArrayMemory0().subarray(ptr / 2, ptr / 2 + len);
}

function getArrayU32FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint32ArrayMemory0().subarray(ptr / 4, ptr / 4 + len);
}

function getArrayU8FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
}

let cachedFloat64ArrayMemory0 = null;
function getFloat64ArrayMemory0() {
    if (cachedFloat64ArrayMemory0 === null || cachedFloat64ArrayMemory0.byteLength === 0) {
        cachedFloat64ArrayMemory0 = new Float64Array(wasm.memory.buffer);
    }
    return cachedFloat64ArrayMemory0;
}

function getStringFromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return decodeText(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}

let cachedUint16ArrayMemory0 = null;
function getUint16ArrayMemory0() {
    if (cachedUint16ArrayMemory0 === null || cachedUint16ArrayMemory0.byteLength === 0) {
        cachedUint16ArrayMemory0 = new Uint16Array(wasm.memory.buffer);
    }
    return cachedUint16ArrayMemory0;
}

let cachedUint32ArrayMemory0 = null;
function getUint32ArrayMemory0() {
    if (cachedUint32ArrayMemory0 === null || cachedUint32ArrayMemory0.byteLength === 0) {
        cachedUint32ArrayMemory0 = new Uint32Array(wasm.memory.buffer);
    }
    return cachedUint32ArrayMemory0;
}

let cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
    if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
        cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
    }
    return cachedUint8ArrayMemory0;
}

function isLikeNone(x) {
    return x === undefined || x === null;
}

function passArray8ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 1, 1) >>> 0;
    getUint8ArrayMemory0().set(arg, ptr / 1);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passStringToWasm0(arg, malloc, realloc) {
    if (realloc === undefined) {
        const buf = cachedTextEncoder.encode(arg);
        const ptr = malloc(buf.length, 1) >>> 0;
        getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
        WASM_VECTOR_LEN = buf.length;
        return ptr;
    }

    let len = arg.length;
    let ptr = malloc(len, 1) >>> 0;

    const mem = getUint8ArrayMemory0();

    let offset = 0;

    for (; offset < len; offset++) {
        const code = arg.charCodeAt(offset);
        if (code > 0x7F) break;
        mem[ptr + offset] = code;
    }
    if (offset !== len) {
        if (offset !== 0) {
            arg = arg.slice(offset);
        }
        ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
        const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
        const ret = cachedTextEncoder.encodeInto(arg, view);

        offset += ret.written;
        ptr = realloc(ptr, len, offset, 1) >>> 0;
    }

    WASM_VECTOR_LEN = offset;
    return ptr;
}

let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true });
cachedTextDecoder.decode();
const MAX_SAFARI_DECODE_BYTES = 2146435072;
let numBytesDecoded = 0;
function decodeText(bytes) {
    numBytesDecoded += bytes.length;
    if (numBytesDecoded >= MAX_SAFARI_DECODE_BYTES) {
        cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true });
        cachedTextDecoder.decode();
        numBytesDecoded = bytes.length;
    }
    return cachedTextDecoder.decode(bytes);
}

const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        const buf = cachedTextEncoder.encode(arg);
        view.set(buf);
        return {
            read: arg.length,
            written: buf.length
        };
    };
}

let WASM_VECTOR_LEN = 0;

let wasmModule, wasmInstance, wasm;
function __wbg_finalize_init(instance, module) {
    wasmInstance = instance;
    wasm = instance.exports;
    wasmModule = module;
    cachedFloat64ArrayMemory0 = null;
    cachedUint16ArrayMemory0 = null;
    cachedUint32ArrayMemory0 = null;
    cachedUint8ArrayMemory0 = null;
    wasm.__wbindgen_start();
    return wasm;
}

async function __wbg_load(module, imports) {
    if (typeof Response === 'function' && module instanceof Response) {
        if (!module.ok) {
            throw new Error(`failed to fetch Wasm: ${module.status} ${module.statusText} fetching '${module.url}'`);
        }

        if (typeof WebAssembly.instantiateStreaming === 'function') {
            try {
                return await WebAssembly.instantiateStreaming(module, imports);
            } catch (e) {
                const validResponse = expectedResponseType(module.type);

                if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                    console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                } else { throw e; }
            }
        }

        const bytes = await module.arrayBuffer();
        return await WebAssembly.instantiate(bytes, imports);
    } else {
        const instance = await WebAssembly.instantiate(module, imports);

        if (instance instanceof WebAssembly.Instance) {
            return { instance, module };
        } else {
            return instance;
        }
    }

    function expectedResponseType(type) {
        switch (type) {
            case 'basic': case 'cors': case 'default': return true;
        }
        return false;
    }
}

function initSync(module) {
    if (wasm !== undefined) return wasm;


    if (module !== undefined) {
        if (Object.getPrototypeOf(module) === Object.prototype) {
            ({module} = module)
        } else {
            console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
        }
    }

    const imports = __wbg_get_imports();
    if (!(module instanceof WebAssembly.Module)) {
        module = new WebAssembly.Module(module);
    }
    const instance = new WebAssembly.Instance(module, imports);
    return __wbg_finalize_init(instance, module);
}

let __wbg_init_promise;
async function __wbg_init(module_or_path) {
    if (wasm !== undefined) return wasm;
    if (__wbg_init_promise === undefined) {
        __wbg_init_promise = __wbg_init_once(module_or_path);
        __wbg_init_promise.catch(() => { __wbg_init_promise = undefined; });
    }
    return __wbg_init_promise;
}

async function __wbg_init_once(module_or_path) {

    if (module_or_path !== undefined) {
        if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
            ({module_or_path} = module_or_path)
        } else {
            console.warn('using deprecated parameters for the initialization function; pass a single object instead')
        }
    }

    if (module_or_path === undefined) {
        module_or_path = new URL('term_web_bg.wasm', import.meta.url);
    }
    const imports = __wbg_get_imports();

    if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
        module_or_path = fetch(module_or_path);
    }

    const { instance, module } = await __wbg_load(await module_or_path, imports);

    return __wbg_finalize_init(instance, module);
}

export { initSync, __wbg_init as default };
