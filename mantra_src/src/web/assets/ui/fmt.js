// Mantra web UI — formatting: markdown subset → vnodes, unified diff colouring, time/token
// formatting, the role/effort colour vocabulary from theme.rs, and a small line-icon set.
//
// Markdown is rendered to vnodes, never to HTML strings: agent output is untrusted, and text
// nodes cannot inject markup. Links are only made clickable for http(s)/mailto.
'use strict';
(function (M) {
    const h = M.h;

    // ── colours (theme.rs `named`) ───────────────────────────────────────────────────────────────
    const NAMED = { saffron: 1, violet: 1, teal: 1, cyan: 1, green: 1, rose: 1, red: 1, amber: 1, blue: 1, gray: 1 };
    function colorVar(name) { return NAMED[name] ? 'var(--' + name + ')' : 'var(--text)'; }
    const EFFORT_COLOR = { minimal: 'muted', low: 'muted', medium: 'teal', high: 'blue', xhigh: 'violet', max: 'saffron', ultra: 'rose' };
    function effortColor(e) { const c = EFFORT_COLOR[e]; return c ? 'var(--' + c + ')' : 'var(--text)'; }

    // ── numbers & time ───────────────────────────────────────────────────────────────────────────
    function tokens(n) {
        n = Number(n) || 0;
        if (n < 1000) return String(n);
        if (n < 100000) return (n / 1000).toFixed(1).replace(/\.0$/, '') + 'k';
        if (n < 1000000) return Math.round(n / 1000) + 'k';
        return (n / 1000000).toFixed(1).replace(/\.0$/, '') + 'M';
    }
    function dur(ms) {
        ms = Math.max(0, Number(ms) || 0);
        const s = Math.floor(ms / 1000);
        if (s < 60) return s + 's';
        const m = Math.floor(s / 60);
        if (m < 60) return m + 'm ' + String(s % 60).padStart(2, '0') + 's';
        const hh = Math.floor(m / 60);
        if (hh < 24) return hh + 'h ' + String(m % 60).padStart(2, '0') + 'm';
        return Math.floor(hh / 24) + 'd ' + (hh % 24) + 'h';
    }
    function durShort(ms) {
        ms = Math.max(0, Number(ms) || 0);
        const s = Math.floor(ms / 1000);
        if (s < 60) return s + 's';
        const m = Math.floor(s / 60);
        if (m < 60) return m + 'm';
        const hh = Math.floor(m / 60);
        if (hh < 24) return hh + 'h' + (m % 60 ? ' ' + (m % 60) + 'm' : '');
        return Math.floor(hh / 24) + 'd';
    }
    // "now" / "12s ago" / "3m ago" / "2h ago" / "Mar 4"
    function ago(at, now) {
        if (!at) return '';
        const d = Math.max(0, (now || Date.now()) - at);
        if (d < 5000) return 'now';
        if (d < 60000) return Math.floor(d / 1000) + 's ago';
        if (d < 3600000) return Math.floor(d / 60000) + 'm ago';
        if (d < 86400000) return Math.floor(d / 3600000) + 'h ago';
        if (d < 7 * 86400000) return Math.floor(d / 86400000) + 'd ago';
        return new Date(at).toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
    }
    function clock(at) {
        if (!at) return '';
        return new Date(at).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });
    }
    function trunc(s, n) { s = String(s || ''); return s.length > n ? s.slice(0, Math.max(0, n - 1)) + '…' : s; }
    function plural(n, one, many) { return n + ' ' + (n === 1 ? one : (many || one + 's')); }

    // ── inline markdown ──────────────────────────────────────────────────────────────────────────
    const SAFE_URL = /^(https?:\/\/|mailto:)/i;
    function inline(text) {
        const out = [];
        const s = String(text);
        let i = 0, buf = '';
        const flush = () => { if (buf) { out.push(buf); buf = ''; } };
        while (i < s.length) {
            const c = s[i];
            if (c === '`') {
                let n = 1;
                while (s[i + n] === '`') n++;
                const fence = '`'.repeat(n);
                const end = s.indexOf(fence, i + n);
                if (end > i + n - 1 && end !== -1) {
                    flush();
                    out.push(h('code', null, s.slice(i + n, end).replace(/^ (.*) $/, '$1')));
                    i = end + n;
                    continue;
                }
            } else if (c === '*' && s[i + 1] === '*') {
                const end = s.indexOf('**', i + 2);
                if (end > i + 2) {
                    flush();
                    out.push(h('strong', null, inline(s.slice(i + 2, end))));
                    i = end + 2;
                    continue;
                }
            } else if (c === '[') {
                const m = /^\[([^\]\n]{1,300})\]\(([^)\s]{1,2000})\)/.exec(s.slice(i, i + 2400));
                if (m) {
                    flush();
                    if (SAFE_URL.test(m[2])) out.push(h('a', { href: m[2], target: '_blank', rel: 'noopener noreferrer' }, inline(m[1])));
                    else out.push(m[0]);
                    i += m[0].length;
                    continue;
                }
            } else if ((c === 'h' || c === 'H') && /^https?:\/\//i.test(s.slice(i, i + 8)) && (i === 0 || /[\s(<]/.test(s[i - 1]))) {
                const m = /^https?:\/\/[^\s<>)\]"']+/i.exec(s.slice(i, i + 2000));
                if (m) {
                    let url = m[0].replace(/[.,;:!?]+$/, '');
                    flush();
                    out.push(h('a', { href: url, target: '_blank', rel: 'noopener noreferrer' }, url));
                    i += url.length;
                    continue;
                }
            }
            buf += c;
            i++;
        }
        flush();
        return out;
    }

    // ── block markdown (md.rs subset + tables) ───────────────────────────────────────────────────
    function markdown(text) {
        const lines = String(text || '').replace(/\r\n?/g, '\n').split('\n');
        const blocks = [];
        let i = 0, k = 0;
        while (i < lines.length) {
            const line = lines[i];
            const fence = /^\s*(```|~~~)\s*([\w+#.-]*)/.exec(line);
            if (fence) {
                const body = [];
                i++;
                while (i < lines.length && !lines[i].trim().startsWith(fence[1])) { body.push(lines[i]); i++; }
                i++; // closing fence (or EOF while streaming)
                const lang = fence[2] || '';
                const isDiff = lang === 'diff' || lang === 'patch';
                blocks.push(h('pre', { key: 'b' + k++, class: 'md-pre' + (isDiff ? ' diff' : ''), 'data-lang': lang || null },
                    isDiff ? diffLines(body.join('\n')) : h('code', null, body.join('\n'))));
                continue;
            }
            if (!line.trim()) { i++; continue; }
            const hd = /^(#{1,6})\s+(.*)$/.exec(line);
            if (hd) {
                const lvl = Math.min(4, hd[1].length + 1);
                blocks.push(h('h' + lvl, { key: 'b' + k++, class: 'md-h' }, inline(hd[2].replace(/\s+#+\s*$/, ''))));
                i++;
                continue;
            }
            if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) { blocks.push(h('hr', { key: 'b' + k++ })); i++; continue; }
            if (/^\s*>/.test(line)) {
                const body = [];
                while (i < lines.length && /^\s*>/.test(lines[i])) { body.push(lines[i].replace(/^\s*>\s?/, '')); i++; }
                blocks.push(h('blockquote', { key: 'b' + k++ }, markdown(body.join('\n'))));
                continue;
            }
            if (/^\s*\|.*\|\s*$/.test(line) && i + 1 < lines.length && /^\s*\|?\s*:?-{2,}/.test(lines[i + 1])) {
                const rows = [];
                while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) { rows.push(lines[i]); i++; }
                const cells = (r) => r.trim().replace(/^\||\|$/g, '').split('|').map((c) => c.trim());
                const head = cells(rows[0]);
                const body = rows.slice(2).map(cells);
                blocks.push(h('div', { key: 'b' + k++, class: 'md-table' }, h('table', null,
                    h('thead', null, h('tr', null, head.map((c) => h('th', null, inline(c))))),
                    h('tbody', null, body.map((r) => h('tr', null, r.map((c) => h('td', null, inline(c)))))))));
                continue;
            }
            const li = /^(\s*)([-*+]|\d{1,3}[.)])\s+(.*)$/.exec(line);
            if (li) {
                const ordered = /\d/.test(li[2]);
                const items = [];
                while (i < lines.length) {
                    const m = /^(\s*)([-*+]|\d{1,3}[.)])\s+(.*)$/.exec(lines[i]);
                    if (m && /\d/.test(m[2]) === ordered) {
                        const depth = Math.min(3, Math.floor(m[1].replace(/\t/g, '    ').length / 2));
                        const task = /^\[([ xX])\]\s+(.*)$/.exec(m[3]);
                        items.push(h('li', { class: depth ? 'd' + depth : null, 'data-n': ordered ? m[2].replace(/[.)]/, '') : null },
                            task ? [h('span', { class: 'task' + (task[1] !== ' ' ? ' done' : '') }, task[1] !== ' ' ? '✓' : ''), ' ', inline(task[2])] : inline(m[3])));
                        i++;
                    } else if (lines[i].trim() && /^\s{2,}\S/.test(lines[i]) && items.length) {
                        // continuation line of the previous item
                        const prev = items[items.length - 1];
                        prev.children.push(' ', ...inline(lines[i].trim()));
                        i++;
                    } else break;
                }
                blocks.push(h(ordered ? 'ol' : 'ul', { key: 'b' + k++ }, items));
                continue;
            }
            const para = [];
            while (i < lines.length && lines[i].trim() && !/^\s*(```|~~~|#{1,6}\s|>|([-*+]|\d{1,3}[.)])\s)/.test(lines[i])) { para.push(lines[i]); i++; }
            if (!para.length) { para.push(line); i++; }
            const kids = [];
            para.forEach((p, j) => { if (j) kids.push(h('br')); kids.push(...inline(p)); });
            blocks.push(h('p', { key: 'b' + k++ }, kids));
        }
        return blocks;
    }

    // ── diffs ────────────────────────────────────────────────────────────────────────────────────
    function diffLines(text) {
        const out = [];
        const lines = String(text || '').split('\n');
        if (lines.length && lines[lines.length - 1] === '') lines.pop();
        for (const l of lines) {
            let cls = 'dl';
            if (l.startsWith('+++') || l.startsWith('---') || l.startsWith('diff ') || l.startsWith('index ') || l.startsWith('new file') || l.startsWith('deleted file')) cls = 'dl meta';
            else if (l.startsWith('@@')) cls = 'dl hunk';
            else if (l.startsWith('+')) cls = 'dl add';
            else if (l.startsWith('-')) cls = 'dl del';
            else if (l.startsWith('…') || l.startsWith('\\')) cls = 'dl note';
            out.push(h('span', { class: cls }, l || ' '));
        }
        return out;
    }

    // ── icons (24×24, stroke) ────────────────────────────────────────────────────────────────────
    const P = {
        team: ['c9,8,3.2', 'M3.5 19.5c.6-3.1 2.8-5 5.5-5s4.9 1.9 5.5 5', 'c17,9.2,2.4', 'M16.2 14.3c2.3.2 3.9 1.9 4.4 4.4'],
        agent: ['M5 4.5h14a2 2 0 0 1 2 2v8.5a2 2 0 0 1-2 2h-8l-4.5 3.5V17H5a2 2 0 0 1-2-2V6.5a2 2 0 0 1 2-2z', 'M8 9.5h8', 'M8 12.5h5'],
        run: ['M12 3.5l8.5 4.5-8.5 4.5L3.5 8z', 'M3.5 12l8.5 4.5 8.5-4.5', 'M3.5 16l8.5 4.5 8.5-4.5'],
        inbox: ['M3.5 13.5l2.8-8h11.4l2.8 8v5a1.5 1.5 0 0 1-1.5 1.5H5a1.5 1.5 0 0 1-1.5-1.5z', 'M3.5 13.5h5l1.2 2.5h4.6l1.2-2.5h5'],
        more: ['c5.5,12,1.3,f', 'c12,12,1.3,f', 'c18.5,12,1.3,f'],
        pulse: ['M3 12h4l2.5-6.5 5 13 2.5-6.5h4'],
        runs: ['M4 6h16', 'M4 12h16', 'M4 18h10'],
        settings: ['M4 7h9', 'M17 7h3', 'c15,7,2', 'M4 17h3', 'M11 17h9', 'c9,17,2', 'M4 12h14', 'M20 12h0'],
        send: ['M4.5 12.5l15-8-5.5 15-2.8-6.2z', 'M11.2 13.3l8.3-8.8'],
        stop: ['r7,7,10,10,2'],
        compact: ['M9 4v5H4', 'M15 4v5h5', 'M9 20v-5H4', 'M15 20v-5h5'],
        respawn: ['M19.5 12a7.5 7.5 0 1 1-2.2-5.3', 'M20 4.5v4.5h-4.5'],
        model: ['r6.5,6.5,11,11,2', 'r10,10,4,4,.5', 'M9.5 3.5v3', 'M14.5 3.5v3', 'M9.5 17.5v3', 'M14.5 17.5v3', 'M3.5 9.5h3', 'M3.5 14.5h3', 'M17.5 9.5h3', 'M17.5 14.5h3'],
        diff: ['M7 3.5h7l5 5v12H7a2 2 0 0 1-2-2v-13a2 2 0 0 1 2-2z', 'M9.5 12h6', 'M12.5 9v6', 'M9.5 17h6'],
        plan: ['M9.5 6.5h10', 'M9.5 12h10', 'M9.5 17.5h10', 'M4.5 6.5l1 1 2-2', 'M4.5 12l1 1 2-2', 'c5.8,17.5,1.1'],
        back: ['M15 5l-7 7 7 7'],
        close: ['M6.5 6.5l11 11', 'M17.5 6.5l-11 11'],
        copy: ['r8.5,8.5,11,11,2', 'M15.5 8.5V6a2 2 0 0 0-2-2H6.5a2 2 0 0 0-2 2v7.5a2 2 0 0 0 2 2h2'],
        bell: ['M6.5 16.5V11a5.5 5.5 0 0 1 11 0v5.5l1.8 1.8H4.7z', 'M10 20.5a2.1 2.1 0 0 0 4 0'],
        check: ['M5 12.5l4.5 4.5L19 7.5'],
        chevron: ['M9.5 5.5L16 12l-6.5 6.5'],
        down: ['M6 9.5l6 6 6-6'],
        up: ['M6 14.5l6-6 6 6'],
        plus: ['M12 5v14', 'M5 12h14'],
        minus: ['M5 12h14'],
        pause: ['M8.5 5.5v13', 'M15.5 5.5v13'],
        play: ['M8 5.5l11 6.5-11 6.5z'],
        land: ['c6.5,5.5,2', 'c6.5,18.5,2', 'c17.5,12,2', 'M6.5 7.5v9', 'M6.5 7.5c0 3.5 3.5 4.5 9 4.5'],
        search: ['c11,11,6.5', 'M20 20l-4.3-4.3'],
        lock: ['r5,10.5,14,10,2', 'M8 10.5V7.5a4 4 0 0 1 8 0v3'],
        globe: ['c12,12,8.5', 'M3.5 12h17', 'M12 3.5c2.8 3 2.8 14 0 17', 'M12 3.5c-2.8 3-2.8 14 0 17'],
        trash: ['M4.5 7h15', 'M9.5 7V4.5h5V7', 'M6.5 7l1 13h9l1-13'],
        download: ['M12 4v11', 'M7 10.5l5 5 5-5', 'M5 20h14'],
        panel: ['r3.5,4.5,17,15,2', 'M15 4.5v15'],
        arrowdown: ['M12 5v14', 'M6.5 13.5l5.5 5.5 5.5-5.5'],
        terminal: ['M5 7l4.5 4.5L5 16', 'M12 17h7'],
        file: ['M7 3.5h7l5 5v12H7a2 2 0 0 1-2-2v-13a2 2 0 0 1 2-2z', 'M14 3.5v5h5'],
        tool: ['M14.5 5.5a4 4 0 0 0 4.9 5l-8.9 8.9a2.1 2.1 0 0 1-3-3l8.9-8.9a4 4 0 0 0-1.9-2z'],
        alert: ['M12 4l9 16H3z', 'M12 10v4.5', 'M12 17.2v.3'],
        question: ['c12,12,8.5', 'M9.6 9.5a2.5 2.5 0 1 1 3.5 2.3c-.7.3-1.1.9-1.1 1.7v.5', 'M12 16.8v.3'],
        logout: ['M14.5 4.5h4a1.5 1.5 0 0 1 1.5 1.5v12a1.5 1.5 0 0 1-1.5 1.5h-4', 'M10 8l-4 4 4 4', 'M6 12h10'],
        eye: ['M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12z', 'c12,12,2.8'],
        eyeoff: ['M3.5 3.5l17 17', 'M10 5.8c.6-.2 1.3-.3 2-.3 6 0 9.5 6.5 9.5 6.5a17 17 0 0 1-2.6 3.3', 'M6.3 7.3A16.5 16.5 0 0 0 2.5 12s3.5 6.5 9.5 6.5c1.6 0 3-.4 4.2-1'],
        dots: ['c5.5,12,1.3,f', 'c12,12,1.3,f', 'c18.5,12,1.3,f'],
        sun: ['c12,12,4', 'M12 2.5v2', 'M12 19.5v2', 'M2.5 12h2', 'M19.5 12h2', 'M5.3 5.3l1.4 1.4', 'M17.3 17.3l1.4 1.4', 'M5.3 18.7l1.4-1.4', 'M17.3 6.7l1.4-1.4'],
        cert: ['r3.5,4.5,17,12,2', 'M7 8.5h10', 'M7 12h6', 'c16.5,16.5,2.5', 'M15 18.5l-.8 3 2.3-1.2 2.3 1.2-.8-3'],
        info: ['c12,12,8.5', 'M12 11v5.5', 'M12 7.8v.3'],
        refresh: ['M19.5 12a7.5 7.5 0 1 1-2.2-5.3', 'M20 4.5v4.5h-4.5'],
        link: ['M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1', 'M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1'],
        edit: ['M4.5 19.5l1-4 10-10 3 3-10 10z', 'M13.5 7.5l3 3'],
        spark: ['M12 3l2 7 7 2-7 2-2 7-2-7-7-2 7-2z'],
    };
    function icon(name, cls) {
        const parts = P[name] || P.info;
        return h('svg', { class: 'ic' + (cls ? ' ' + cls : ''), viewBox: '0 0 24 24', 'aria-hidden': 'true', focusable: 'false' },
            parts.map((d) => {
                if (d[0] === 'c') { const a = d.slice(1).split(','); return h('circle', { cx: a[0], cy: a[1], r: a[2], class: a[3] === 'f' ? 'fill' : null }); }
                if (d[0] === 'r') { const a = d.slice(1).split(','); return h('rect', { x: a[0], y: a[1], width: a[2], height: a[3], rx: a[4] || 0 }); }
                return h('path', { d });
            }));
    }

    M.fmt = { colorVar, effortColor, tokens, dur, durShort, ago, clock, trunc, plural, inline, markdown, diffLines, icon };
})(window.Mantra = window.Mantra || {});
