// 操作员签名（多人协作插件）：新建会话时带上“谁开的”。名字先用这个浏览器里自己填的，没填用登录账户的昵称。
// 存不存由那台电脑决定（装了并授权多人协作插件才存），这里只管带上。

const KEY = 'looklook.operator';
export const OPERATOR_MAX = 32;
let cloud = '';

export function cleanOperator(s) {
  return [...String(s ?? '').replace(/[\u0000-\u001f\u007f]/g, ' ').trim()].slice(0, OPERATOR_MAX).join('').trim();
}

/** 这个浏览器里自己填的名字（没填是空）。 */
export function ownOperator() {
  try {
    return cleanOperator(localStorage.getItem(KEY));
  } catch {
    return '';
  }
}

export function setOwnOperator(name) {
  const v = cleanOperator(name);
  try {
    if (v) localStorage.setItem(KEY, v);
    else localStorage.removeItem(KEY);
  } catch {
    /* 存不了就只这次不带 */
  }
  window.dispatchEvent(new Event('ll-operator'));
}

/** 登录账户的昵称（App 拿到 status 后告诉这里）。 */
export function setCloudOperator(name) {
  const v = cleanOperator(name);
  if (v === cloud) return;
  cloud = v;
  window.dispatchEvent(new Event('ll-operator'));
}

/** 现在用的名字：自己填的优先，其次账户昵称。 */
export function operatorName() {
  return ownOperator() || cloud;
}

/** 新建会话的请求体加上签名（已经带了的不动）。 */
export function withOperator(method, path, body) {
  if (method !== 'POST' || path !== '/instances' || !body || typeof body !== 'object' || body.created_by) return body;
  const by = operatorName();
  return by ? { ...body, created_by: by } : body;
}
