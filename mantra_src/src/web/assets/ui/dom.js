// Mantra web UI — a tiny keyed virtual-DOM: h(tag, props, ...children) and patch(parent, vnodes).
//
// Why not a library: no build step, no CDN (CSP), and the app needs exactly this much. Rules:
// - a vnode is {tag, props, children, key} or a string (text node); null/false/true are skipped
// - children are matched by `key` when given, else by position among unkeyed siblings of the
//   same tag, so inputs keep focus and caret across re-renders
// - an identical vnode object (===) is skipped entirely: memoised subtrees (transcript items)
//   cost one comparison per render
// - `ref(el)` runs once when the element is created; `raw: true` leaves the children alone
//   (for elements that own their contents, e.g. a <canvas> or a textarea's value)
'use strict';
(function (M) {
    const SVG = 'http://www.w3.org/2000/svg';
    const EMPTY = {};
    const PROP_KEYS = { value: 1, checked: 1, selected: 1, indeterminate: 1 };

    function flat(out, c) {
        if (c === null || c === undefined || c === false || c === true) return out;
        if (Array.isArray(c)) { for (const x of c) flat(out, x); return out; }
        if (typeof c === 'number') { out.push(String(c)); return out; }
        out.push(c);
        return out;
    }

    function h(tag, props, ...children) {
        props = props || EMPTY;
        return { tag, props, children: flat([], children), key: props.key };
    }

    function setProp(el, name, val, old, svg) {
        if (name === 'key' || name === 'ref' || name === 'raw') return;
        if (name.charCodeAt(0) === 111 && name.charCodeAt(1) === 110) { // on…
            const ev = name.slice(2).toLowerCase();
            const hs = el.__h || (el.__h = {});
            if (!hs[ev] && val) el.addEventListener(ev, dispatch);
            hs[ev] = val || null;
            return;
        }
        if (name === 'class') {
            if (svg) { if (val) el.setAttribute('class', val); else el.removeAttribute('class'); }
            else el.className = val || '';
            return;
        }
        if (name === 'style') {
            if (typeof val === 'string' || !val) { el.style.cssText = val || ''; return; }
            const o = (old && typeof old === 'object') ? old : EMPTY;
            for (const k in o) if (!(k in val)) { if (k.startsWith('--')) el.style.removeProperty(k); else el.style[k] = ''; }
            for (const k in val) if (val[k] !== o[k]) { if (k.startsWith('--')) el.style.setProperty(k, val[k]); else el.style[k] = val[k]; }
            return;
        }
        if (PROP_KEYS[name] && !svg) {
            if (el[name] !== val) el[name] = val === undefined || val === null ? (name === 'value' ? '' : false) : val;
            return;
        }
        if (val === false || val === null || val === undefined) el.removeAttribute(name);
        else el.setAttribute(name, val === true ? '' : String(val));
    }

    function dispatch(ev) {
        const f = this.__h && this.__h[ev.type];
        if (f) f(ev, this);
    }

    function create(v, svg) {
        if (typeof v === 'string') return document.createTextNode(v);
        const isSvg = svg || v.tag === 'svg';
        const el = isSvg ? document.createElementNS(SVG, v.tag) : document.createElement(v.tag);
        for (const k in v.props) setProp(el, k, v.props[k], undefined, isSvg);
        if (!v.props.raw) for (const c of v.children) el.appendChild(create(c, isSvg && v.tag !== 'foreignObject'));
        el.__v = v;
        if (v.props.ref) v.props.ref(el);
        return el;
    }

    function update(el, v, svg) {
        const old = el.__v;
        if (old === v) return;
        const isSvg = svg || v.tag === 'svg';
        const op = old ? old.props : EMPTY, np = v.props;
        for (const k in op) if (!(k in np)) setProp(el, k, undefined, op[k], isSvg);
        for (const k in np) if (np[k] !== op[k] || PROP_KEYS[k]) setProp(el, k, np[k], op[k], isSvg);
        el.__v = v;
        if (!np.raw) children(el, v.children, isSvg && v.tag !== 'foreignObject');
    }

    function same(el, v) {
        if (typeof v === 'string') return el.nodeType === 3;
        return el.nodeType === 1 && el.__v && el.__v.tag === v.tag;
    }

    function children(parent, vs, svg) {
        const nodes = parent.childNodes;
        let keyed = null;
        const unkeyed = [];
        for (let i = 0; i < nodes.length; i++) {
            const n = nodes[i];
            const k = n.__v && n.__v.key;
            if (k !== undefined && k !== null) (keyed || (keyed = new Map())).set(k, n);
            else unkeyed.push(n);
        }
        let u = 0;
        for (let i = 0; i < vs.length; i++) {
            const v = vs[i];
            let el = null;
            const k = typeof v === 'string' ? undefined : v.key;
            if (k !== undefined && k !== null) {
                const c = keyed && keyed.get(k);
                if (c && same(c, v)) { el = c; keyed.delete(k); }
            } else {
                // next unkeyed old node of a compatible type
                while (u < unkeyed.length && !same(unkeyed[u], v)) u++;
                if (u < unkeyed.length) { el = unkeyed[u]; unkeyed[u] = null; u++; }
            }
            if (el) {
                if (typeof v === 'string') { if (el.nodeValue !== v) el.nodeValue = v; }
                else update(el, v, svg);
            } else el = create(v, svg);
            const cur = nodes[i];
            if (cur !== el) parent.insertBefore(el, cur || null);
        }
        while (nodes.length > vs.length) parent.removeChild(nodes[nodes.length - 1]);
    }

    // Patch `parent`'s children to match `vnodes` (a vnode or an array of them).
    function patch(parent, vnodes) {
        children(parent, flat([], vnodes), parent instanceof SVGElement);
    }

    M.h = h;
    M.patch = patch;
})(window.Mantra = window.Mantra || {});
