// URL Shortener — generates short codes, stores in KV
// Tests: kv, crypto.randomUUID, string manipulation

export function shorten(url) {
    if (!url || !url.startsWith("http")) {
        throw new Error("Invalid URL: must start with http");
    }
    const code = crypto.randomUUID().slice(0, 8);
    kv.set("url:" + code, url);
    kv.set("clicks:" + code, "0");
    return { code, shortUrl: "https://short.app/" + code, originalUrl: url };
}

export function resolve(code) {
    const url = kv.get("url:" + code);
    if (!url) return null;
    // Increment click counter
    const clicks = parseInt(kv.get("clicks:" + code) || "0") + 1;
    kv.set("clicks:" + code, String(clicks));
    return { url, clicks };
}

export function stats(code) {
    const url = kv.get("url:" + code);
    if (!url) return null;
    const clicks = parseInt(kv.get("clicks:" + code) || "0");
    return { code, url, clicks };
}

export function list() {
    return kv.list()
        .filter(k => k.startsWith("url:"))
        .map(k => {
            const code = k.slice(4);
            return {
                code,
                url: kv.get(k),
                clicks: parseInt(kv.get("clicks:" + code) || "0"),
            };
        });
}
