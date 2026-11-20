((invoke, emit, catalog, read, write) => {
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
    const call = (name, args) => invoke(name, stringify(args));
    const helpers = {
        ls: (args = {}) => parse(call("ls", args)),
        search: args => parse(call("search", args)),
        repo_map: (args = {}) => parse(call("repo_map", args)),
        read: args => call("read", args)
    };
    const entries = Object.freeze(catalog.map(entry => Object.freeze(entry)));
    globalThis.tools = Object.freeze(Object.fromEntries(entries.map(({name}) => [name, helpers[name]])));
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
        if (namespace) return []; // The native workspace helpers have no namespace.

        const terms = new Map();
        for (const term of new Set(words(query))) {
            const frequency = documents.filter(doc => doc.names.has(term) || doc.counts.has(term)).length;
            if (frequency) terms.set(term, Math.log1p((documents.length - frequency + 0.5) / (frequency + 0.5)));
        }
        const exactName = query.trim().toLowerCase();
        const matches = [];
        for (const {entry, names, counts, length} of documents) {
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
    globalThis.text = value => emit(typeof value === "string" ? value : stringify(value));
})(globalThis.__invoke, globalThis.__text, globalThis.__catalog, globalThis.__load, globalThis.__store);
delete globalThis.__load;
delete globalThis.__store;
delete globalThis.__catalog;
delete globalThis.__invoke;
delete globalThis.__text;
