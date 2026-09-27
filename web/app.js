const accountsNode = document.querySelector('#accounts');
const summaryNode = document.querySelector('#summary');
const errorNode = document.querySelector('#error');
const reloadButton = document.querySelector('#reload');
const paginationNode = document.querySelector('#pagination');
const previousButton = document.querySelector('#previous');
const nextButton = document.querySelector('#next');
const pageInfoNode = document.querySelector('#page-info');
const pageSize = 30;
let accounts = [];
let page = 0;
let loading = false;
let reloadAfterLoad = false;
const busyAccounts = new Set();
const actionErrors = new Map();

function element(tag, className, value) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (value !== undefined) node.textContent = value;
  return node;
}

function timestamp(value) {
  if (value === null || value === undefined || value === '') return null;
  if (typeof value === 'number') return value < 1e12 ? value * 1000 : value;
  if (/^\d+$/.test(value)) return timestamp(Number(value));
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? null : parsed;
}

function dateTime(value) {
  const time = timestamp(value);
  return time === null ? '' : new Intl.DateTimeFormat('zh-CN', {
    timeZone: 'Asia/Shanghai', year: 'numeric', month: '2-digit', day: '2-digit',
    hour: '2-digit', minute: '2-digit', hourCycle: 'h23',
  }).format(time).replaceAll('/', '-');
}

function relative(value) {
  const time = timestamp(value);
  if (time === null) return '';
  const delta = time - Date.now();
  const amount = Math.abs(delta);
  const unit = amount >= 86_400_000 ? '天' : amount >= 3_600_000 ? '小时' : '分钟';
  const divisor = unit === '天' ? 86_400_000 : unit === '小时' ? 3_600_000 : 60_000;
  return `${Math.max(1, Math.floor(amount / divisor))}${unit}${delta >= 0 ? '后' : '前'}`;
}

function windowTitle(windowData) {
  const seconds = windowData.window_seconds;
  const prefix = windowData.key.split(':', 1)[0];
  let period = seconds === 18_000 ? '5 小时窗口' : seconds === 604_800 ? '7 天窗口' : '';
  if (!period && seconds > 0) {
    period = seconds % 86_400 === 0 ? `${seconds / 86_400} 天窗口`
      : seconds % 3_600 === 0 ? `${seconds / 3_600} 小时窗口`
      : `${Math.ceil(seconds / 60)} 分钟窗口`;
  }
  if (!period) period = windowData.key.endsWith('secondary_window') ? '次级窗口' : '主窗口';
  return prefix === 'codex' ? period : `${prefix} · ${period}`;
}

function renderWindow(windowData) {
  const row = element('div', 'window');
  const line = element('div', 'window-line');
  line.append(element('span', 'window-title', windowTitle(windowData)));
  const used = windowData.used_percent;
  const remaining = typeof used === 'number' ? Math.max(0, Math.min(100, 100 - used)) : null;
  line.append(element('span', 'window-value', remaining === null ? '未知' : `${Math.round(remaining)}%`));
  if (windowData.reset_at_ms !== null) {
    line.append(element('span', 'window-reset', `${dateTime(windowData.reset_at_ms)} · ${relative(windowData.reset_at_ms)}`));
  }
  row.append(line);
  const bar = element('div', 'bar');
  const fill = element('div', `fill ${remaining < 30 ? 'low' : remaining < 70 ? 'medium' : ''}`);
  fill.style.width = `${remaining ?? 0}%`;
  bar.append(fill);
  row.append(bar);
  return row;
}

function renderAccount(account) {
  const card = element('article', 'account');
  const heading = element('div', 'heading');
  const plan = String(account.plan || '未知').trim();
  const lower = plan.toLowerCase();
  const tone = lower.includes('team') ? 'team' : lower.includes('pro') ? 'pro' : lower.includes('plus') ? 'plus' : 'neutral';
  heading.append(element('span', `plan ${tone}`, plan.toUpperCase()));
  const title = element('span', 'account-name', account.email || account.name || account.id);
  title.title = account.email || account.name || account.id;
  heading.append(title);
  const states = { ready: '正常', expired: '凭据过期', banned: '已封禁', invalid: '凭据无效', unknown: '待验证' };
  const status = account.enabled ? states[account.credentialState] : '已停用';
  const statusButton = element('button', `status ${account.enabled && account.credentialState === 'ready' ? '' : 'off'}`, `● ${status}`);
  statusButton.type = 'button';
  statusButton.title = account.enabled ? '停用账号调度' : '启用账号调度';
  statusButton.setAttribute('aria-label', statusButton.title);
  statusButton.disabled = busyAccounts.has(account.id);
  statusButton.addEventListener('click', () => accountAction(account, 'api/status'));
  heading.append(statusButton);
  const refreshButton = element('button', 'account-refresh', '↻');
  refreshButton.type = 'button';
  refreshButton.title = account.enabled ? '主动刷新额度' : '已停用账号不能刷新额度';
  refreshButton.setAttribute('aria-label', `刷新 ${account.email || account.name || account.id} 的额度`);
  refreshButton.disabled = !account.enabled || busyAccounts.has(account.id);
  refreshButton.addEventListener('click', () => accountAction(account, 'api/refresh'));
  heading.append(refreshButton);
  card.append(heading);

  const expiry = element('div', 'expiry');
  expiry.append(element('span', 'label', '令牌中的套餐到期'));
  const end = timestamp(account.subscriptionUntil);
  const toneDate = end === null ? '' : end <= Date.now() ? 'expired' : end - Date.now() <= 86_400_000 ? 'soon' : 'valid';
  expiry.append(element('span', `date ${toneDate}`, dateTime(account.subscriptionUntil) || '未知'));
  if (end !== null) expiry.append(element('span', 'relative', `· ${relative(account.subscriptionUntil)}`));
  card.append(expiry);

  if (account.windows.length) account.windows.forEach((windowData) => card.append(renderWindow(windowData)));
  else card.append(element('p', 'empty', '等待被动额度数据'));
  if (account.error) card.append(element('p', 'foot', account.error));
  else if (account.observedAtMs !== null) card.append(element('p', 'foot', `额度采集于 ${dateTime(account.observedAtMs)}`));
  if (actionErrors.has(account.id)) card.append(element('p', 'foot action-error', actionErrors.get(account.id)));
  return card;
}

async function accountAction(account, path) {
  if (busyAccounts.has(account.id)) return;
  busyAccounts.add(account.id);
  actionErrors.delete(account.id);
  renderPage();
  try {
    const reply = await window.codexProxyPlugin.request({
      method: 'POST', path, contentType: 'application/json',
      body: JSON.stringify({ accountId: account.id }),
    });
    const payload = JSON.parse(new TextDecoder().decode(reply.body));
    if (reply.status !== 200) throw new Error(payload.error || `操作失败（${reply.status}）`);
    if (loading) reloadAfterLoad = true;
    else await refresh();
  } catch (error) {
    actionErrors.set(account.id, error.message);
  } finally {
    busyAccounts.delete(account.id);
    renderPage();
  }
}

function renderPage() {
  const pageCount = Math.ceil(accounts.length / pageSize);
  page = Math.min(page, Math.max(0, pageCount - 1));
  accountsNode.replaceChildren(...accounts.slice(page * pageSize, (page + 1) * pageSize).map(renderAccount));
  if (!accounts.length) accountsNode.append(element('p', 'empty', '暂无 Codex OAuth 账号'));
  paginationNode.hidden = pageCount <= 1;
  pageInfoNode.textContent = `第 ${page + 1} / ${pageCount} 页 · 每页 ${pageSize} 个`;
  previousButton.disabled = page === 0;
  nextButton.disabled = page >= pageCount - 1;
}

async function refresh() {
  if (loading || document.hidden) return;
  loading = true;
  reloadButton.disabled = true;
  try {
    const reply = await window.codexProxyPlugin.request({ method: 'GET', path: 'api/accounts' });
    if (reply.status !== 200) throw new Error(`读取失败（${reply.status}）`);
    const payload = JSON.parse(new TextDecoder().decode(reply.body));
    accounts = payload.accounts;
    renderPage();
    summaryNode.textContent = `${accounts.length} 个账号 · 每 30 秒读取一次缓存`;
    errorNode.hidden = true;
  } catch (error) {
    errorNode.textContent = `读取账号失败：${error.message}`;
    errorNode.hidden = false;
  } finally {
    loading = false;
    reloadButton.disabled = false;
    if (reloadAfterLoad) {
      reloadAfterLoad = false;
      void refresh();
    }
  }
}

function applyTheme() {
  document.documentElement.dataset.theme = window.codexProxyPlugin?.theme || 'light';
}

reloadButton.addEventListener('click', refresh);
previousButton.addEventListener('click', () => { page--; renderPage(); });
nextButton.addEventListener('click', () => { page++; renderPage(); });
document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });
window.addEventListener('codex-proxy-themechange', applyTheme);
applyTheme();
refresh();
window.setInterval(refresh, 30_000);
