// Dashboard copy helpers (curl / OpenAI config). Served at /static/app.js so
// the page needs no inline script under its Content-Security-Policy.
function showToast(msg) {
  const t = document.getElementById('toast');
  t.textContent = msg;
  t.classList.add('show');
  clearTimeout(window.__toastTimer);
  window.__toastTimer = setTimeout(() => t.classList.remove('show'), 1800);
}
function baseUrl(row) {
  let ep = row.dataset.endpoint || '';
  if (!ep) {
    if (row.dataset.source === 'openrouter') ep = 'https://openrouter.ai/api/v1';
    else ep = 'https://api.openai.com/v1';
  }
  ep = ep.replace(/\/$/, '');
  if (!/\/v1$/i.test(ep)) ep = ep + '/v1';
  return ep;
}
function curlFor(row) {
  const base = baseUrl(row);
  const model = row.dataset.model || '';
  const body = JSON.stringify({
    model,
    messages: [{ role: 'user', content: 'hello' }]
  });
  return `curl -sS "${base}/chat/completions" \\
  -H "Authorization: Bearer $API_KEY" \\
  -H "Content-Type: application/json" \\
  -d '${body.replace(/'/g, "'\\''")}'`;
}
function openaiConfigFor(row) {
  const base = baseUrl(row);
  const model = row.dataset.model || '';
  const provider = row.dataset.provider || '';
  return JSON.stringify({
    provider,
    model,
    base_url: base,
    api_key_env: 'API_KEY',
    openai_python: `from openai import OpenAI\\nclient = OpenAI(base_url="${base}", api_key=os.environ["API_KEY"])\\nr = client.chat.completions.create(model="${model}", messages=[{"role":"user","content":"hello"}])`
  }, null, 2);
}
async function copyText(text, label) {
  try {
    await navigator.clipboard.writeText(text);
    showToast('Copied ' + label);
  } catch (e) {
    // fallback
    const ta = document.createElement('textarea');
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand('copy');
    document.body.removeChild(ta);
    showToast('Copied ' + label);
  }
}
document.addEventListener('click', (ev) => {
  const btn = ev.target.closest('[data-copy]');
  if (!btn) return;
  const row = btn.closest('tr.quote-row');
  if (!row) return;
  const kind = btn.getAttribute('data-copy');
  if (kind === 'curl') copyText(curlFor(row), 'curl');
  else if (kind === 'openai') copyText(openaiConfigFor(row), 'OpenAI config');
});
