((invoke, batch, asyncMode, emit, catalog, read, write, emitImage) => {
    const stringify = JSON.stringify, parse = JSON.parse;
    globalThis.load = key => {
        if (typeof key !== "string") throw new TypeError("load() key must be a string");
        const json = read(key);
        return json === "" ? undefined : parse(json);
    };
    globalThis.store = (key, value) => {
        if (typeof key !== "string") throw new TypeError("store() key must be a string");
        const json = stringify(value);
        if (json === undefined && value !== undefined) throw new TypeError("store() requires a JSON value");
        write(key, json);
    };
    const entries = Object.freeze(catalog.map(entry => Object.freeze(entry)));
    const decode = (entry, value) => {
        const kind = entry.output ?? (entry.name === "read" ? "file" : "json");
        if (kind === "text") return value;
        if (kind === "json") return parse(value);
        const {text, image} = parse(value);
        if (!image) return text;
        return Object.freeze({type: "image", mimeType: image.slice(5, image.indexOf(";")), data: image.slice(image.indexOf(",")+1), note:text});
    };
    const pending = [];
    const makeTools = asynchronous => Object.freeze(Object.fromEntries(entries.map(entry => [entry.name,
        (args = {}) => asynchronous ? new Promise((resolve,reject) => {
            if (pending.length >= 64) throw Error("Codemode nested call limit exceeded");
            // Snapshot arguments before yielding to script code.
            pending.push({entry,args:parse(stringify(args)),resolve,reject});
        }) : decode(entry,invoke(entry.name,stringify(args)))
    ])));
    globalThis.asyncTools = makeTools(true);
    globalThis.tools = asyncMode ? globalThis.asyncTools : makeTools(false);
    globalThis.__flushTools = () => {
        if (!pending.length) return false;
        const group = pending.splice(0,4);
        const replies = parse(batch(stringify(group.map(({entry,args}) => [entry.name,args]))));
        group.forEach(({entry,resolve,reject},i) => {
            try {
                const result = replies[i];
                if ("Err" in result) throw Error(result.Err);
                resolve(decode(entry,result.Ok));
            } catch (error) { reject(error); }
        });
        return true;
    };
    globalThis.describeNamespace = name => {
        if (typeof name !== "string") throw new TypeError("describeNamespace() expects a string");
        const members = entries.filter(e => (e.namespace ?? "workspace") === name);
        return members.length ? members : undefined;
    };
    globalThis.ALL_TOOLS = entries;
    const descriptions = new Map(entries.map(({name, description}) => [name, description]));
    globalThis.describeTool = name => {
        if (typeof name !== "string") throw new TypeError("describeTool() expects a tool name");
        return descriptions.get(name);
    };

    const words = value => value.replace(/([a-z])([A-Z])/g, "$1 $2").toLowerCase().match(/[a-z0-9]+/g) ?? [];
    const documents = entries.map(entry => {
        const tokens = words(entry.description);
        const counts = new Map();
        for (const token of tokens) counts.set(token, (counts.get(token) ?? 0) + 1);
        return {entry, names: new Set(words(entry.name)), counts, length: tokens.length};
    });
    const average = documents.reduce((sum, doc) => sum + doc.length, 0) / documents.length || 1;
    globalThis.searchTools = (query, options) => {
        if (typeof query !== "string") throw new TypeError("searchTools() expects a query string");
        const limit = options?.limit ?? 8;
        if (!Number.isInteger(limit) || limit <= 0) throw new TypeError("limit must be a positive integer");
        const namespace = options?.namespace;
        if (namespace != null && typeof namespace !== "string") throw new TypeError("namespace must be a string");


        const terms = new Map();
        for (const term of new Set(words(query))) {
            const frequency = documents.filter(doc => doc.names.has(term) || doc.counts.has(term)).length;
            if (frequency) terms.set(term, Math.log1p((documents.length - frequency + 0.5) / (frequency + 0.5)));
        }
        const exactName = query.trim().toLowerCase();
        const matches = [];
        for (const {entry, names, counts, length} of documents) {
            if (namespace && (entry.namespace ?? "workspace") !== namespace) continue;
            let covered = 0, score = 0;
            // BM25 length normalization, with extra weight for terms in the tool name.
            const norm = 1.2 * (0.25 + 0.75 * length / average);
            for (const [term, idf] of terms) {
                const count = counts.get(term) ?? 0;
                if (!count && !names.has(term)) continue;
                covered++;
                score += idf * ((names.has(term) ? 3 : 0) + 2.2 * count / (count + norm));
            }
            if (covered) matches.push({entry, covered, score, exact: entry.name === exactName});
        }
        matches.sort((a, b) => b.exact - a.exact || b.covered - a.covered || b.score - a.score);
        return matches.slice(0, limit).map(match => match.entry);
    };
    globalThis.image = value => {
        const url = value?.type === "image" ? "data:" + value.mimeType + ";base64," + value.data : value?.image_url?.url ?? value?.image_url ?? value;
        if (typeof url !== "string" || !url.startsWith("data:image/")) throw new TypeError("image() requires a local image data URL or image block");
        emitImage(url);
    };
    globalThis.text = value => emit(value?.type === "image" && typeof value.note === "string" ? value.note : typeof value === "string" ? value : stringify(value), false);
    function formatArgument(value) {
        if (typeof value === "string") return value;
        if (value instanceof Error) {
            const message = String(value);
            const stack = value.stack;
            return typeof stack === "string" && stack ? message + "\n" + stack.trimEnd() : message;
        }
        if (typeof value === "number") return String(value);
        try {
            return stringify(value) ?? String(value);
        } catch {
            return String(value);
        }
    }
    const log = (...args) => emit(args.map(formatArgument).join(" "), true);
    globalThis.console = Object.freeze({ log, info: log, warn: log, error: log, debug: log });
})(globalThis.__invoke, globalThis.__batch, globalThis.__asyncMode, globalThis.__emit, globalThis.__catalog, globalThis.__load, globalThis.__store, globalThis.__image);
delete globalThis.__batch;
delete globalThis.__asyncMode;
delete globalThis.__image;
delete globalThis.__load;
delete globalThis.__store;
delete globalThis.__catalog;
delete globalThis.__invoke;
delete globalThis.__emit;
