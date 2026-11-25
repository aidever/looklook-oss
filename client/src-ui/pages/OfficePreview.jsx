// 文件栏的办公文件预览（docs/PLAN_2026-10-27.md §6.1；docx / pptx / 旧格式见 docs/FILES_REDO_DESIGN_2026-11-24.md §4.4.1）：
// PDF 用浏览器自带的查看器；Word（.docx）用 docx-preview 按页排版，PowerPoint（.pptx）用 @aiden0z/pptx-renderer；
// 旧版 .doc / .ppt 先在浏览器里用 x2t 转成 docx / pptx（shared/x2t.js）；Excel：.xlsx 用 read-excel-file，.xls / .xlsb / .ods
// 用 SheetJS，都只显示数据；CSV / TSV 自己解析。这些库都在用到时才加载（动态 import），不进首屏。表格只显示前 MAX_ROWS 行。
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { colName, MAX_ROWS } from '../shared/office.js';
import { convertLegacy } from '../shared/x2t.js';

/**
 * 把取回的文件转成要显示的内容：{ url }（PDF，用完要 revoke）、{ docx: ArrayBuffer }、{ pptx: ArrayBuffer }、
 * { sheets: [{ name, rows, total }] }。`ctx`（旧格式转换用）：{ ops, device, onStage, signal }。
 */
export async function renderOffice(kind, blob, ctx = {}) {
  if (kind === 'pdf') return { url: URL.createObjectURL(new Blob([blob], { type: 'application/pdf' })) };
  if (kind === 'docx' || kind === 'pptx') return { [kind]: await blob.arrayBuffer() };
  if (kind === 'doc' || kind === 'ppt') {
    const out = await convertLegacy(await blob.arrayBuffer(), kind, ctx);
    return { [out.kind]: out.buf };
  }
  if (kind === 'xls') {
    const XLSX = await import('xlsx');
    const wb = XLSX.read(await blob.arrayBuffer(), { sheetRows: MAX_ROWS, cellDates: true, dense: true });
    return {
      sheets: wb.SheetNames.map((name) => {
        const ws = wb.Sheets[name];
        const rows = XLSX.utils.sheet_to_json(ws, { header: 1, raw: false, defval: '', blankrows: true });
        const full = ws['!fullref'] ?? ws['!ref'];
        return { name, rows, total: full ? XLSX.utils.decode_range(full).e.r + 1 : rows.length };
      }),
    };
  }
  const { default: readXlsxFile } = await import('read-excel-file/browser');
  const sheets = await readXlsxFile(blob);
  return { sheets: sheets.map(({ sheet, data }) => ({ name: sheet, rows: data.slice(0, MAX_ROWS), total: data.length })) };
}

/** 文档里的链接：只留 http(s) / mailto（新标签打开）和文档内跳转，其余（javascript: 等）去掉 */
function safeLinks(root) {
  for (const el of root.querySelectorAll('script, iframe, object, embed')) el.remove();
  for (const a of root.querySelectorAll('a[href]')) {
    const href = a.getAttribute('href');
    if (href.startsWith('#')) continue;
    if (/^(https?:|mailto:)/i.test(href)) {
      a.target = '_blank';
      a.rel = 'noopener noreferrer';
    } else a.removeAttribute('href');
  }
}

/** 点到链接：文档内跳转照常；http(s) / mailto 新标签打开；别的不动（幻灯片是看到哪页才渲染哪页，事后清理不全，在点击时拦） */
function guardLinks(e) {
  const a = e.target instanceof Element ? e.target.closest('a[href]') : null;
  const href = a?.getAttribute('href');
  if (!href || href.startsWith('#')) return;
  e.preventDefault();
  if (/^(https?:|mailto:)/i.test(href)) window.open(href, '_blank', 'noopener,noreferrer');
}

/** Word：按页排版（docx-preview），页面比容器宽时整体缩小（手机上一页能看全） */
export function DocxView({ buf, onError }) {
  const ref = useRef(null);
  useEffect(() => {
    const host = ref.current;
    let alive = true;
    let ro = null;
    const fit = () => {
      const w = host.querySelector('.docx-wrapper');
      if (!w) return;
      w.style.zoom = '';
      const need = w.scrollWidth;
      const have = host.clientWidth;
      if (need > have + 1) w.style.zoom = String(have / need);
    };
    import('docx-preview')
      .then(({ renderAsync }) =>
        renderAsync(buf.slice(0), host, null, { className: 'docx', inWrapper: true, breakPages: true, ignoreLastRenderedPageBreak: false, useBase64URL: true, renderAltChunks: false }),
      )
      .then(() => {
        if (!alive) return;
        safeLinks(host);
        fit();
        ro = new ResizeObserver(fit);
        ro.observe(host);
      })
      .catch((e) => alive && onError?.(e));
    return () => {
      alive = false;
      ro?.disconnect();
      host.replaceChildren();
    };
  }, [buf]);
  return <div ref={ref} className="fb-docx" onClickCapture={guardLinks} />;
}

/** PowerPoint：一页页往下排（@aiden0z/pptx-renderer），宽度跟着容器；只渲染看得见附近的几页 */
export function PptxView({ buf, onError }) {
  const ref = useRef(null);
  useEffect(() => {
    const host = ref.current;
    const ac = new AbortController();
    let viewer = null;
    import('@aiden0z/pptx-renderer')
      .then(({ PptxViewer, RECOMMENDED_ZIP_LIMITS }) =>
        PptxViewer.open(buf.slice(0), host, {
          zipLimits: RECOMMENDED_ZIP_LIMITS,
          fitMode: 'contain',
          // 不去取文档里引用的外部图片 / 视频（会把“有人打开了”泄露出去）
          externalMedia: 'block',
          pdfjs: false,
          scrollContainer: host.closest('.fb-preview-body') ?? undefined,
          renderMode: 'list',
          listOptions: { windowed: true },
          lazyMedia: true,
          lazySlides: true,
          signal: ac.signal,
        }),
      )
      .then((v) => {
        if (ac.signal.aborted) v.destroy();
        else viewer = v;
      })
      .catch((e) => !ac.signal.aborted && onError?.(e));
    return () => {
      ac.abort();
      viewer?.destroy();
      host.replaceChildren();
    };
  }, [buf]);
  // 留白放在外层：渲染器按挂载点的 clientWidth（含 padding）算页宽，padding 放在挂载点上会把页面右边挤出去
  return (
    <div className="fb-pptx" onClickCapture={guardLinks}>
      <div ref={ref} />
    </div>
  );
}

function cellText(v) {
  if (v == null) return '';
  if (v instanceof Date) return v.getUTCHours() || v.getUTCMinutes() ? v.toLocaleString() : v.toLocaleDateString();
  return String(v);
}

/** 表格：像电子表格那样带列名、行号；多个工作表时上面一排标签 */
export function SheetView({ sheets }) {
  const { t } = useTranslation();
  const [n, setN] = useState(0);
  const sheet = sheets[Math.min(n, sheets.length - 1)];
  const cols = sheet.rows.reduce((m, r) => Math.max(m, r.length), 0);
  return (
    <>
      {sheets.length > 1 && (
        <div className="fb-sheets" role="tablist">
          {sheets.map((s, i) => (
            <button key={s.name} type="button" role="tab" aria-selected={i === n} className={i === n ? 'on' : ''} onClick={() => setN(i)}>
              {s.name}
            </button>
          ))}
        </div>
      )}
      {sheet.total > sheet.rows.length && <p className="muted small fb-pad fb-note">{t('fb.rows_truncated', { n: sheet.rows.length, total: sheet.total })}</p>}
      {sheet.rows.length === 0 ? (
        <p className="muted small fb-pad">{t('fb.sheet_empty')}</p>
      ) : (
        <table className="fb-sheet">
          <thead>
            <tr>
              <th />
              {Array.from({ length: cols }, (_, c) => (
                <th key={c}>{colName(c)}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {sheet.rows.map((r, i) => (
              // eslint-disable-next-line react/no-array-index-key -- 行没有别的标识，内容不会重排
              <tr key={i}>
                <th>{i + 1}</th>
                {Array.from({ length: cols }, (_, c) => (
                  <td key={c}>{cellText(r[c])}</td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </>
  );
}
