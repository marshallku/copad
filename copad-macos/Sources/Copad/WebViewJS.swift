import Foundation

/// JavaScript snippets for the webview-interaction socket commands. Mirrors
/// `copad-linux/src/webview.rs::js` snippet-for-snippet so the wire result shape is identical.
/// Snippets return JSON-as-string to keep types stable across the bridge (no NSNumber/NSString
/// coercion).
enum WebViewJS {
    static func querySelector(_ selector: String) -> String {
        let sel = jsString(selector)
        return """
        (() => {
            const el = document.querySelector(\(sel));
            if (!el) return JSON.stringify(null);
            const r = el.getBoundingClientRect();
            return JSON.stringify({
                tag: el.tagName.toLowerCase(),
                text: el.innerText?.slice(0, 2000) || "",
                value: el.value || null,
                href: el.href || null,
                src: el.src || null,
                class: el.className || "",
                id: el.id || "",
                rect: { x: r.x, y: r.y, w: r.width, h: r.height },
                visible: r.width > 0 && r.height > 0,
            });
        })()
        """
    }

    static func querySelectorAll(_ selector: String, limit: Int) -> String {
        let sel = jsString(selector)
        return """
        (() => {
            const els = [...document.querySelectorAll(\(sel))].slice(0, \(limit));
            return JSON.stringify(els.map((el, i) => {
                const r = el.getBoundingClientRect();
                return {
                    index: i,
                    tag: el.tagName.toLowerCase(),
                    text: el.innerText?.slice(0, 500) || "",
                    value: el.value || null,
                    href: el.href || null,
                    class: el.className || "",
                    id: el.id || "",
                    rect: { x: r.x, y: r.y, w: r.width, h: r.height },
                };
            }));
        })()
        """
    }

    static func getStyles(_ selector: String, properties: [String]) -> String {
        let sel = jsString(selector)
        let propsJSON = jsArray(properties)
        return """
        (() => {
            const el = document.querySelector(\(sel));
            if (!el) return JSON.stringify(null);
            const cs = getComputedStyle(el);
            const props = \(propsJSON);
            const result = {};
            props.forEach(p => result[p] = cs.getPropertyValue(p));
            return JSON.stringify(result);
        })()
        """
    }

    static func click(_ selector: String) -> String {
        let sel = jsString(selector)
        return """
        (() => {
            const el = document.querySelector(\(sel));
            if (!el) return JSON.stringify({ ok: false, error: "not found" });
            el.click();
            return JSON.stringify({ ok: true });
        })()
        """
    }

    /// Dispatches `input` and `change` so React-style listeners see the value.
    static func fill(_ selector: String, value: String) -> String {
        let sel = jsString(selector)
        let val = jsString(value)
        return """
        (() => {
            const el = document.querySelector(\(sel));
            if (!el) return JSON.stringify({ ok: false, error: "not found" });
            el.focus();
            el.value = \(val);
            el.dispatchEvent(new Event('input', { bubbles: true }));
            el.dispatchEvent(new Event('change', { bubbles: true }));
            return JSON.stringify({ ok: true });
        })()
        """
    }

    static func scroll(selector: String?, x: Int, y: Int) -> String {
        if let sel = selector {
            let s = jsString(sel)
            return """
            (() => {
                const el = document.querySelector(\(s));
                if (!el) return JSON.stringify({ ok: false, error: "not found" });
                el.scrollIntoView({ behavior: "smooth", block: "center" });
                return JSON.stringify({ ok: true });
            })()
            """
        } else {
            return """
            (() => {
                window.scrollTo(\(x), \(y));
                return JSON.stringify({ ok: true, scrollX: window.scrollX, scrollY: window.scrollY });
            })()
            """
        }
    }

    static func pageInfo() -> String {
        """
        (() => {
            return JSON.stringify({
                title: document.title,
                url: location.href,
                width: document.documentElement.scrollWidth,
                height: document.documentElement.scrollHeight,
                viewportWidth: window.innerWidth,
                viewportHeight: window.innerHeight,
                scrollX: window.scrollX,
                scrollY: window.scrollY,
                forms: document.forms.length,
                links: document.links.length,
                images: document.images.length,
                inputs: document.querySelectorAll('input, textarea, select').length,
                buttons: document.querySelectorAll('button, [role="button"], input[type="submit"]').length,
            });
        })()
        """
    }

    // MARK: - Helpers

    /// JSON-encode a string for embedding in JS source. A failure falls back to `""` so one bad
    /// selector doesn't crash the whole webview command.
    private static func jsString(_ s: String) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: s, options: [.fragmentsAllowed]),
              let str = String(data: data, encoding: .utf8)
        else {
            return "\"\""
        }
        return str
    }

    private static func jsArray(_ items: [String]) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: items),
              let str = String(data: data, encoding: .utf8)
        else {
            return "[]"
        }
        return str
    }
}
