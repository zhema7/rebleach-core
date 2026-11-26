// Rebleach 前端：连接页 / 会话页
// 视频：本地 WebSocket 收 H.264 Annex-B → WebCodecs(VideoDecoder) 硬解 → canvas
// 输入：canvas 键鼠事件 → 换算远程坐标 → invoke('send_input')
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const canvas = $('screen');
const ctx = canvas.getContext('2d', { alpha: false });

/** 日志同时打到 WebView console 与 Rust 日志（打包后 console 不可见） */
function webLog(level, msg) {
  (level === 'error' ? console.error : level === 'warn' ? console.warn : console.log)(msg);
  invoke('js_log', { level, msg: String(msg) }).catch(() => {});
}

const state = {
  active: false,
  mediaPort: 0,
  ws: null,
  wsRetry: 0,
  decoder: null,
  configured: false,
  spsHex: null,
  ts: 0,
  lastMove: 0,
  frames: 0,
  fps: 0,
  loggedFirst: false,
  sendingFile: false,
  clipOn: true,
  fullscreen: false,
  ctl: null, // 主控端配置 { id, nodes, active }
  monitors: [], // 被控端显示器清单（含虚拟桌面原点，用于跨屏拖拽）
  curMonitor: -1, // 当前正在观看的显示器索引
  buttons: new Set(), // 当前按下的鼠标键（拖拽时改用绝对坐标）
  dragging: false,
  switchPending: false,
  link: null,
};

// ============================================================
// 输入记忆（断开/重启后仍在，方便再次连接）
// ============================================================
const LS = {
  peer: 'rebleach.peer',
  fps: 'rebleach.fps',
  ice: 'rebleach.ice',
  lanIp: 'rebleach.lanIp',
  encode: 'rebleach.encodeWidth',
};

function loadSettings() {
  const pick = (key, el, fallback) => {
    const v = localStorage.getItem(key);
    if (v !== null && v !== '') el.value = v;
    else if (fallback !== undefined && !el.value) el.value = fallback;
  };
  pick(LS.peer, $('peer'));
  pick(LS.fps, $('fps'), '30');
  pick(LS.ice, $('ice'));
  pick(LS.lanIp, $('lan-ip'));
  pick(LS.encode, $('encode-select'));
}

function saveSettings(args) {
  localStorage.setItem(LS.peer, args.peer || '');
  localStorage.setItem(LS.fps, String(args.fps));
  localStorage.setItem(LS.ice, args.ice || '');
}

// ============================================================
// 启动
// ============================================================
async function boot() {
  state.mediaPort = await invoke('media_port').catch(() => 0);
  loadSettings();

  await listen('status', (ev) => {
    const s = String(ev.payload ?? '');
    webLog('info', '[status] ' + s);
    if (state.active) $('status-text').textContent = s;
  });

  // 会话结束（主控被踢/断线）→ 自动回主页面，输入内容保留
  await listen('session-ended', (ev) => {
    const reason = String(ev.payload ?? '');
    webLog('info', '[session-ended] ' + reason);
    if (!state.active) return;
    leaveSession();
    showError(reason ? `连接已结束：${reason}` : '连接已结束');
  });

  // 被控端回传的操作结果（增删虚拟屏成功/失败）
  await listen('peer-log', (ev) => {
    const { level, msg } = ev.payload || {};
    const mark = level === 'error' ? '❌' : level === 'warn' ? '⚠️' : 'ℹ️';
    webLog(level === 'error' ? 'error' : level === 'warn' ? 'warn' : 'info', `[peer] ${msg}`);
    if (state.active && msg) {
      $('status-text').textContent = `${mark} ${msg}`;
    }
  });

  // 被控端推送的驱动状态（增删屏后自动推送，让主控端知道还能不能继续操作）
  await listen('driver-status', (ev) => {
    const { has_driver, active_count, driver_kind, hint } = ev.payload || {};
    if (!has_driver) {
      webLog('warn', `[driver] ⚠️ ${hint || '被控端无虚拟屏驱动'}`);
      if (state.active) $('status-text').textContent = '⚠️ ' + (hint || '被控端无虚拟屏驱动');
    } else {
      webLog('info', `[driver] ✅ ${driver_kind} 已就绪（活动 ${active_count} 块）`);
    }
  });

  // 被控端推送的显示器清单 → 填充切换下拉（保留原点用于跨屏拖拽）
  await listen('monitors', (ev) => {
    const list = ev.payload || [];
    state.monitors = list;
    webLog('info', `[monitors] ${list.length} 台显示器`);
    const sel = $('monitor-select');
    const prev = state.curMonitor >= 0 ? state.curMonitor : null;
    sel.innerHTML = '';
    for (const m of list) {
      const opt = document.createElement('option');
      opt.value = m.index;
      const virtTag = m.is_virtual ? ' 💻' : '';
      opt.textContent = `#${m.index} ${m.name || ''} ${m.width}×${m.height}${m.primary ? '（主）' : ''}${virtTag}`;
      sel.appendChild(opt);
    }
    sel.disabled = !list.length;
    if (list.length) {
      // 采集默认从清单第一块屏开始，首次同步到 list[0]，避免拖拽绝对坐标算错
      const idx = prev !== null && list.some((m) => m.index === prev)
        ? prev
        : list[0].index;
      sel.value = String(idx);
      state.curMonitor = idx;
    } else {
      state.curMonitor = -1;
    }
  });

  // 被控端链路上报：编码 fps / 发送码率 / DataChannel 缓冲
  await listen('netstats', (ev) => {
    const p = ev.payload || {};
    const fps = Math.round(p.fps || 0);
    // fps 极低时给出明确提示，而不是只显示一个卡住的画面
    const hint = fps > 0 && fps < 2 ? ' · ⚠️ 对端系统负载过高' : '';
    $('netstats').textContent =
      `远端 ${fps} fps · ${Math.round(p.kbps || 0)} kbps · 缓冲 ${Math.round((p.buffered || 0) / 1024)} KB${hint}`;
  });

  // 当前链路：局域网直连 / 直连 / 中转 + RTT
  await listen('link-info', (ev) => {
    const p = ev.payload || {};
    state.link = p;
    renderLink();
  });

  // 发现新版本 → 点亮工具栏更新按钮
  await listen('update-available', (ev) => {
    const v = String(ev.payload || '');
    webLog('info', `[update] 发现新版本 v${v}`);
    const btn = $('update-btn');
    btn.textContent = `更新到 v${v}`;
    btn.hidden = false;
  });

  $('connect-form').addEventListener('submit', onConnectSubmit);
  $('disconnect-btn').addEventListener('click', () => leaveSession());

  bindToolbar();
  bindInput();
  bindCtl();
  bindAccess();
  await bindTerm();
  startFpsMeter();
  await initCtl();
  await initAccess();
}

/** 状态栏链路信息：局域网直连 / 直连 / 中转 + RTT */
function renderLink() {
  const p = state.link;
  const el = $('link-info');
  if (!p) {
    el.textContent = '';
    el.title = '';
    return;
  }
  const label = p.kind === 'lan' ? '局域网直连' : p.kind === 'relay' ? '中转' : '直连';
  const mark = p.kind === 'lan' ? '🟢' : p.kind === 'relay' ? '🟡' : '🔵';
  el.textContent = `${mark} ${label} · ${Math.round(p.rtt_ms || 0)}ms`;
  el.title = p.detail || '';
}

// 帧率必须由定时器算：WGC 只在画面变化时出帧，静止时没有新帧进来，
// 若在解码回调里更新，数字会停在最后一次的值（看着像 29fps，其实早就没帧了）
function startFpsMeter() {
  let last = performance.now();
  setInterval(() => {
    const now = performance.now();
    const dt = now - last;
    last = now;
    if (!state.configured) {
      state.frames = 0;
      return;
    }
    state.fps = Math.round((state.frames * 1000) / dt);
    state.frames = 0;
    $('stats').textContent = `${state.fps} fps · ${canvas.width}×${canvas.height}`;
  }, 1000);
}

// ============================================================
// 主控端配置：本机 ID + 中转节点管理 + 局域网直连
// ============================================================
async function initCtl() {
  try {
    const cfg = await invoke('ctl_config');
    state.ctl = cfg;
    $('device-id').value = cfg.id || '';
    renderNodeSelect();
    renderNodeList();
  } catch (err) {
    webLog('warn', '读取主控端配置失败: ' + err);
  }
}

/** 连接页的节点下拉（选中项即当前使用节点） */
function renderNodeSelect() {
  const sel = $('node-select');
  const cfg = state.ctl;
  if (!cfg) return;
  sel.innerHTML = '';
  for (const n of cfg.nodes) {
    const opt = document.createElement('option');
    opt.value = n.url;
    opt.textContent = `${n.name}（${n.url}）`;
    sel.appendChild(opt);
  }
  sel.value = cfg.active;
  sel.onchange = async () => {
    state.ctl.active = sel.value;
    await saveCtl();
  };
}

/** 节点管理卡的列表：选中切换 / 删除非内置节点 */
function renderNodeList() {
  const ul = $('node-list');
  const cfg = state.ctl;
  if (!cfg) return;
  ul.innerHTML = '';
  for (const n of cfg.nodes) {
    const li = document.createElement('li');
    const label = document.createElement('span');
    label.textContent = `${n.url === cfg.active ? '● ' : '○ '}${n.name}（${n.url}）`;
    label.style.cursor = 'pointer';
    label.title = '点击切换为该节点';
    label.onclick = async () => {
      state.ctl.active = n.url;
      await saveCtl();
    };
    li.appendChild(label);
    if (!n.builtin) {
      const del = document.createElement('button');
      del.className = 'tb-btn danger';
      del.textContent = '删除';
      del.onclick = async () => {
        state.ctl.nodes = state.ctl.nodes.filter((x) => x.url !== n.url);
        await saveCtl();
      };
      li.appendChild(del);
    }
    ul.appendChild(li);
  }
}

async function saveCtl() {
  try {
    state.ctl = await invoke('ctl_save', { cfg: state.ctl });
  } catch (err) {
    $('node-error').textContent = '节点保存失败：' + String(err);
  }
  renderNodeSelect();
  renderNodeList();
  refreshAccessInfo();
}

function bindCtl() {
  $('copy-id').addEventListener('click', async () => {
    const id = $('device-id').value;
    if (!id) return;
    try {
      await navigator.clipboard.writeText(id);
    } catch (_) {
      $('device-id').select();
      document.execCommand('copy');
    }
  });

  $('node-form').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    $('node-error').textContent = '';
    const name = $('node-name').value.trim();
    const url = $('node-url').value.trim();
    if (!name || !url) {
      $('node-error').textContent = '请填写节点名称与地址';
      return;
    }
    if (!/^wss?:\/\//.test(url)) {
      $('node-error').textContent = '节点地址需以 ws:// 或 wss:// 开头';
      return;
    }
    if (state.ctl.nodes.some((n) => n.url === url)) {
      $('node-error').textContent = '该节点已存在';
      return;
    }
    state.ctl.nodes.push({ name, url, builtin: false });
    state.ctl.active = url;
    $('node-name').value = '';
    $('node-url').value = '';
    await saveCtl();
  });

  $('lan-scan-btn').addEventListener('click', async () => {
    const ul = $('lan-list');
    ul.innerHTML = '<li>正在搜索…</li>';
    try {
      const devs = await invoke('lan_discover');
      ul.innerHTML = '';
      if (!devs.length) {
        ul.innerHTML = '<li>未发现设备（确认被控端已启动且在同一局域网）</li>';
        return;
      }
      for (const d of devs) {
        const li = document.createElement('li');
        const a = document.createElement('span');
        a.className = 'lan-item';
        a.textContent = `${d.hostname || '设备'} · ID ${d.id} · ${d.addr}`;
        a.onclick = () => {
          $('lan-ip').value = d.addr;
          localStorage.setItem(LS.lanIp, d.addr);
        };
        li.appendChild(a);
        ul.appendChild(li);
      }
    } catch (err) {
      ul.innerHTML = '<li>搜索失败：' + String(err) + '</li>';
    }
  });

  $('lan-form').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const ip = $('lan-ip').value.trim();
    const pin = $('lan-pin').value.trim();
    if (!ip) return ($('lan-error').textContent = '请输入被控端 IP');
    if (!/^\d{6}$/.test(pin)) return ($('lan-error').textContent = '请输入 6 位数字验证码');
    $('lan-error').textContent = '';
    localStorage.setItem(LS.lanIp, ip);
    const btn = $('lan-connect-btn');
    btn.disabled = true;
    const args = {
      server: `ws://${ip}:7681/ws`,
      id: $('device-id').value.trim(),
      role: 'controller',
      peer: 'lan',
      fps: Number($('fps').value) || 30,
      ice: $('ice').value.trim() || null,
      auth: pin,
    };
    try {
      await invoke('connect', { args });
    } catch (err) {
      $('lan-error').textContent = String(err);
      btn.disabled = false;
      return;
    }
    btn.disabled = false;
    enterSession(args);
  });
}

// ============================================================
// 接入与 SSH：服务端凭据 / 公钥登记 / SSH 配置 / 安装命令
// ============================================================
async function initAccess() {
  const cfg = state.ctl;
  if (cfg) {
    $('access-admin-user').value = cfg.admin_user || '';
    $('access-admin-pass').value = cfg.admin_pass || '';
  }
  await refreshAccessInfo();
}

/** 刷新服务器地址与主控端公钥（切换节点后同步） */
async function refreshAccessInfo() {
  try {
    const info = await invoke('access_info');
    $('access-server').value = info.server_http || info.server || '';
    $('access-pubkey').value = info.pubkey || '';
    if (!$('access-admin-user').value) {
      $('access-admin-user').value = info.admin_user || '';
    }
  } catch (err) {
    setAccessStatus('读取接入信息失败：' + String(err));
  }
}

function setAccessStatus(msg) {
  $('access-status').textContent = msg;
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (_) {
    return false;
  }
}

/** 保存管理员凭据（接口失败时返回 false，避免后续调用无谓报错） */
async function saveAdminCreds() {
  if (!state.ctl) {
    setAccessStatus('主控端配置未就绪，请稍候重试');
    return false;
  }
  const user = $('access-admin-user').value.trim();
  const pass = $('access-admin-pass').value;
  if (!user || !pass) {
    setAccessStatus('请填写管理员账号与密码');
    return false;
  }
  state.ctl.admin_user = user;
  state.ctl.admin_pass = pass;
  try {
    state.ctl = await invoke('ctl_save', { cfg: state.ctl });
  } catch (err) {
    setAccessStatus('保存凭据失败：' + String(err));
    return false;
  }
  renderNodeSelect();
  renderNodeList();
  return true;
}

function bindAccess() {
  $('access-copy-pub').addEventListener('click', async () => {
    const key = $('access-pubkey').value;
    if (!key) return;
    setAccessStatus((await copyText(key)) ? '公钥已复制' : '复制失败，请手动选择文本');
  });

  $('access-register-key').addEventListener('click', async () => {
    if (!(await saveAdminCreds())) return;
    setAccessStatus('正在登记公钥…');
    try {
      await invoke('access_register_key');
      setAccessStatus('公钥已登记（被控端将在 60 秒内按授权生效）');
    } catch (err) {
      setAccessStatus('登记失败：' + String(err));
    }
  });

  $('access-save-admin').addEventListener('click', async () => {
    if (await saveAdminCreds()) setAccessStatus('管理员凭据已保存到本机');
  });

  $('access-ssh-config').addEventListener('click', async () => {
    if (!(await saveAdminCreds())) return;
    setAccessStatus('正在生成 SSH 配置…');
    try {
      const r = await invoke('access_write_ssh_config', { backend: 'builtin', sshUser: null });
      const aliases = r.aliases || [];
      if (!aliases.length) {
        setAccessStatus(`已写入 ${r.path}（当前没有生效授权，配置为空）`);
      } else {
        setAccessStatus(`已写入 ${r.path}，可用：${aliases.map((a) => 'ssh ' + a).join(' / ')}`);
      }
    } catch (err) {
      setAccessStatus('生成失败：' + String(err));
    }
  });

  $('access-new-token').addEventListener('click', async () => {
    if (!(await saveAdminCreds())) return;
    setAccessStatus('正在生成安装令牌…');
    try {
      const r = await invoke('access_new_enroll_token', {
        note: $('access-note').value.trim(),
        ttlSecs: Number($('access-ttl').value) || 3600,
      });
      $('access-cmd').value = r.command;
      $('access-cmd-box').hidden = false;
      setAccessStatus('安装命令已生成，请在被控端 PowerShell 中执行');
    } catch (err) {
      setAccessStatus('生成失败：' + String(err));
    }
  });

  $('access-copy-cmd').addEventListener('click', async () => {
    const cmd = $('access-cmd').value;
    if (!cmd) return;
    setAccessStatus((await copyText(cmd)) ? '安装命令已复制' : '复制失败，请手动选择文本');
  });
}

// ============================================================
// 连接 / 断开
// ============================================================
async function onConnectSubmit(ev) {
  ev.preventDefault();
  const btn = $('connect-btn');
  const args = {
    server: $('node-select').value,
    id: $('device-id').value.trim(),
    role: 'controller',
    peer: $('peer').value.trim() || null,
    fps: Number($('fps').value) || 30,
    ice: $('ice').value.trim() || null,
    auth: null,
  };

  if (!args.server) return showError('请选择中转节点');
  if (!args.id) return showError('主控端配置未就绪，请稍候重试');
  if (!args.peer) return showError('请输入被控端设备 ID');

  showError('');
  btn.disabled = true;
  try {
    await invoke('connect', { args });
  } catch (err) {
    showError(String(err));
    btn.disabled = false;
    return;
  }
  btn.disabled = false;
  saveSettings(args);
  enterSession(args);
}

function showError(msg) {
  $('connect-error').textContent = msg;
}

function enterSession(args) {
  state.active = true;
  $('connect-view').hidden = true;
  $('session-view').hidden = false;
  $('stats').textContent = '';
  $('netstats').textContent = '';
  $('link-info').textContent = '';
  $('file-status').textContent = '';
  state.link = null;
  state.monitors = [];
  state.curMonitor = -1;
  state.buttons.clear();
  state.dragging = false;
  state.switchPending = false;

  termReset();
  $('toolbar').hidden = false;
  const sel = $('monitor-select');
  sel.innerHTML = '<option value="-1">等待清单…</option>';
  sel.disabled = true;

  // 把上次保存的画质（编码宽度）同步给新会话，避免主控端显示 720p 而实际仍是 1080p
  setTimeout(() => {
    if (!state.active) return;
    sendControl({
      t: 'set_quality',
      level: 'keep',
      bitrate_kbps: 0,
      max_width: Number($('encode-select').value) || 0,
    });
  }, 1500);
  $('hint').hidden = false;
  $('hint').textContent = '等待画面…';
  if (state.mediaPort) {
    openMediaSocket();
  } else {
    $('status-text').textContent = '媒体端口不可用';
  }
  canvas.focus();
  webLog('info', `连接 ${args.peer} @ ${args.server}`);
}

/** 返回主页面（保留表单里上次输入的内容，便于再次连接） */
async function leaveSession() {
  if (!state.active) {
    $('session-view').hidden = true;
    $('connect-view').hidden = false;
    return;
  }
  state.active = false;
  if (state.fullscreen) await setFullscreen(false);
  closeMediaSocket();
  resetDecoder();
  await invoke('disconnect').catch(() => {});
  $('session-view').hidden = true;
  $('connect-view').hidden = false;
  $('toolbar').hidden = true;
  $('status-text').textContent = '';
  $('stats').textContent = '';
  $('netstats').textContent = '';
  $('link-info').textContent = '';
  $('file-status').textContent = '';
  state.link = null;
  state.monitors = [];
  state.curMonitor = -1;
  state.buttons.clear();
  state.dragging = false;
  termReset();
  $('peer').focus();
  $('peer').select();
}

// ============================================================
// 媒体 WebSocket
// ============================================================
function openMediaSocket() {
  const ws = new WebSocket(`ws://127.0.0.1:${state.mediaPort}/stream`);
  ws.binaryType = 'arraybuffer';
  state.ws = ws;

  ws.onopen = () => {
    state.wsRetry = 0;
    webLog('info', 'media ws open');
  };

  ws.onmessage = (ev) => {
    if (ev.data instanceof ArrayBuffer) {
      handleVideoPacket(new Uint8Array(ev.data));
    }
  };

  ws.onclose = () => {
    if (state.ws !== ws) return;
    if (!state.active) return;
    webLog('warn', 'media ws closed');
    if (state.wsRetry >= 5) {
      $('status-text').textContent = '媒体通道已断开';
      return;
    }
    state.wsRetry += 1;
    setTimeout(() => {
      if (state.active) openMediaSocket();
    }, 400);
  };
}

function closeMediaSocket() {
  const ws = state.ws;
  state.ws = null;
  if (ws) {
    ws.onmessage = null;
    ws.onclose = null;
    ws.close();
  }
}

// ============================================================
// H.264 → VideoDecoder
// 包格式：[1B key][Annex-B NAL...]
// ============================================================
function handleVideoPacket(packet) {
  if (packet.length < 2) return;
  const key = packet[0] !== 0;
  const bytes = packet.subarray(1);

  if (!state.configured) {
    // 只靠关键帧里的 SPS/PPS 完成 configure
    if (!key) return;
    if (!configureDecoder(bytes)) return;
  } else if (key) {
    // 显示器切换 / 分辨率变化 → SPS 变化 → 必须重配解码器，否则花屏
    const hex = spsHexOf(bytes);
    if (hex && state.spsHex && hex !== state.spsHex) {
      webLog('info', 'SPS 变化，重新配置解码器');
      resetDecoder();
      if (!configureDecoder(bytes)) return;
    }
  }

  const nals = splitAnnexB(bytes);
  if (!nals.length) return;

  // 参数集 / AUD 已进 description，逐帧只送 VCL
  const vcl = [];
  let total = 0;
  for (const nal of nals) {
    const t = nal[0] & 0x1f;
    if (t === 7 || t === 8 || t === 9) continue;
    vcl.push(nal);
    total += 4 + nal.length;
  }
  if (!vcl.length) return;

  // 解码队列堆积时丢 P 帧保实时性
  if (state.decoder.decodeQueueSize > 8 && !key) return;

  const data = new Uint8Array(total);
  const view = new DataView(data.buffer);
  let off = 0;
  for (const nal of vcl) {
    view.setUint32(off, nal.length, false);
    off += 4;
    data.set(nal, off);
    off += nal.length;
  }

  // 时间戳只需单调递增（以 30fps 为基准，仅作占位）
  state.ts += 33333;
  try {
    state.decoder.decode(
      new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: state.ts, data })
    );
  } catch (err) {
    webLog('error', 'decode: ' + (err && err.message ? err.message : err));
  }
}

function configureDecoder(bytes) {
  if (typeof VideoDecoder === 'undefined') {
    $('status-text').textContent = '当前 WebView 不支持 WebCodecs';
    return false;
  }
  const sps = findNal(bytes, 7);
  const pps = findNal(bytes, 8);
  if (!sps || !pps) return false; // 等下一个关键帧

  const hex = (v) => v.toString(16).padStart(2, '0');
  const codec = `avc1.${hex(sps[1])}${hex(sps[2])}${hex(sps[3])}`;
  const desc = new Uint8Array(11 + sps.length + pps.length);
  desc.set([1, sps[1], sps[2], sps[3], 0xff, 0xe1, 0, 0], 0);
  new DataView(desc.buffer).setUint16(6, sps.length, false);
  desc.set(sps, 8);
  const ppsOff = 8 + sps.length;
  desc[ppsOff] = 1;
  new DataView(desc.buffer).setUint16(ppsOff + 1, pps.length, false);
  desc.set(pps, ppsOff + 3);

  const decoder = new VideoDecoder({
    output: onVideoFrame,
    error: (e) => {
      webLog('error', 'decoder error: ' + e.message);
      $('status-text').textContent = '解码失败: ' + e.message;
    },
  });
  try {
    decoder.configure({ codec, description: desc, optimizeForLatency: true });
  } catch (e) {
    webLog('error', `configure failed (${codec}): ` + e.message);
    $('status-text').textContent = `解码器配置失败 (${codec})`;
    return false;
  }

  state.decoder = decoder;
  state.configured = true;
  state.spsHex = Array.from(sps).map(hex).join('');
  state.ts = 0;
  webLog('info', `configure ${codec} sps=${sps.length} pps=${pps.length}`);
  return true;
}

function resetDecoder() {
  if (state.decoder && state.decoder.state !== 'closed') {
    try { state.decoder.close(); } catch (_) {}
  }
  state.decoder = null;
  state.configured = false;
  state.spsHex = null;
  state.ts = 0;
  state.frames = 0;
  state.fps = 0;
  state.loggedFirst = false;
  ctx.fillStyle = '#000';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
}

function onVideoFrame(frame) {
  try {
    if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
      canvas.width = frame.displayWidth;
      canvas.height = frame.displayHeight;
    }
    ctx.drawImage(frame, 0, 0);
    state.frames += 1;
    if (!state.loggedFirst) {
      state.loggedFirst = true;
      webLog('info', `first frame ${frame.displayWidth}x${frame.displayHeight}`);
      $('status-text').textContent = '已连接 · 实时画面';
    }
    $('hint').hidden = true;
  } finally {
    frame.close();
  }
}

/** Annex-B 切分为 NAL（去掉起始码与尾部补零） */
function splitAnnexB(bytes) {
  const nals = [];
  const n = bytes.length;
  let i = 0;
  let start = -1;
  const push = (end) => {
    let e = end;
    while (e > start && bytes[e - 1] === 0) e -= 1;
    if (e > start) nals.push(bytes.subarray(start, e));
  };
  while (i + 2 < n) {
    if (bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 1) {
      if (start >= 0) push(i);
      i += 3;
      start = i;
    } else {
      i += 1;
    }
  }
  if (start >= 0) push(n);
  return nals;
}

function findNal(bytes, type) {
  for (const nal of splitAnnexB(bytes)) {
    if (nal.length > 1 && (nal[0] & 0x1f) === type) return nal;
  }
  return null;
}

/** 提取关键帧里 SPS 的 hex 指纹，用于检测分辨率/显示器切换 */
function spsHexOf(bytes) {
  const sps = findNal(bytes, 7);
  if (!sps) return null;
  return Array.from(sps).map((v) => v.toString(16).padStart(2, '0')).join('');
}

// ============================================================
// 输入回传
// ============================================================
function sendControl(msg) {
  invoke('send_input', { msg }).catch(() => {});
}

function remotePoint(ev) {
  const r = canvas.getBoundingClientRect();
  if (!r.width || !r.height || !canvas.width || !canvas.height) return null;
  const nx = Math.min(Math.max(0, (ev.clientX - r.left) / r.width), 1);
  const ny = Math.min(Math.max(0, (ev.clientY - r.top) / r.height), 1);
  return { x: Math.round(nx * (canvas.width - 1)), y: Math.round(ny * (canvas.height - 1)) };
}

const BUTTON_MAP = { 0: 0, 2: 1, 1: 2 }; // 左 / 右 / 中 → 协议 0/1/2

function monitorByIndex(idx) {
  return state.monitors.find((m) => m.index === idx) || null;
}

/**
 * 拖拽时的绝对坐标：
 * - 把 canvas 上的归一化位置换算成当前显示器的虚拟桌面物理坐标；
 * - 指针越过当前屏边界时，找包含该点的相邻显示器并自动切换采集，
 *   这样就能把窗口从一块屏拖到另一块屏（文档 P1 跨屏拖拽）。
 */
function dragPoint(ev) {
  const r = canvas.getBoundingClientRect();
  if (!r.width || !r.height) return null;
  const m = monitorByIndex(state.curMonitor)
    || state.monitors.find((x) => x.primary)
    || state.monitors[0];
  if (!m) return null;
  const nx = (ev.clientX - r.left) / r.width;
  const ny = (ev.clientY - r.top) / r.height;
  const ox = Number(m.origin_x || 0);
  const oy = Number(m.origin_y || 0);
  const abx = Math.round(ox + nx * m.width);
  const aby = Math.round(oy + ny * m.height);

  const outside =
    abx < ox || abx >= ox + m.width || aby < oy || aby >= oy + m.height;
  if (outside && !state.switchPending) {
    const next = state.monitors.find(
      (t) =>
        abx >= Number(t.origin_x || 0) &&
        abx < Number(t.origin_x || 0) + t.width &&
        aby >= Number(t.origin_y || 0) &&
        aby < Number(t.origin_y || 0) + t.height
    );
    if (next && next.index !== m.index) {
      webLog('info', `拖拽跨屏 → #${next.index}`);
      state.curMonitor = next.index;
      state.switchPending = true;
      $('monitor-select').value = String(next.index);
      sendControl({ t: 'switch_monitor', index: next.index });
      setTimeout(() => {
        state.switchPending = false;
      }, 900);
    }
  }
  return { x: abx, y: aby };
}

function bindInput() {
  canvas.addEventListener('pointermove', (ev) => {
    if (!state.active) return;
    const now = performance.now();
    const dragging = state.buttons.size > 0;
    // M1：拖拽同样限速 60Hz（旧版拖拽不限速，跨境会话会洪泛输入通道加剧滞后）
    if (now - state.lastMove < 16) return;
    state.lastMove = now;
    if (dragging && state.monitors.length) {
      const p = dragPoint(ev);
      if (p) sendControl({ t: 'mouse_move_abs', x: p.x, y: p.y });
      return;
    }
    const p = remotePoint(ev);
    if (!p) return;
    sendControl({ t: 'mouse_move', x: p.x, y: p.y });
  });

  canvas.addEventListener('pointerdown', (ev) => {
    if (!state.active) return;
    ev.preventDefault();
    canvas.focus();
    try { canvas.setPointerCapture(ev.pointerId); } catch (_) {}
    state.buttons.add(ev.button);
    sendControl({ t: 'mouse_down', b: BUTTON_MAP[ev.button] ?? 0 });
  });

  const release = (ev) => {
    if (!state.active) return;
    if (!state.buttons.has(ev.button)) return;
    state.buttons.delete(ev.button);
    if (!state.buttons.size) {
      try { canvas.releasePointerCapture(ev.pointerId); } catch (_) {}
    }
    sendControl({ t: 'mouse_up', b: BUTTON_MAP[ev.button] ?? 0 });
  };
  canvas.addEventListener('pointerup', release);
  canvas.addEventListener('pointercancel', release);

  window.addEventListener('mouseup', (ev) => {
    if (!state.active) return;
    // pointerup 已处理过则不重复发送；这里只兜底"pointerup 没收到"的情况
    if (!state.buttons.has(ev.button)) return;
    state.buttons.clear();
    sendControl({ t: 'mouse_up', b: BUTTON_MAP[ev.button] ?? 0 });
  });

  // 滚轮：浏览器 deltaY 正 = 向下滚；被控端 enigo 的 scroll 正数 = 向下滚。
  // 之前发了 -deltaY 且是像素量（enigo 内部再乘 120），方向反了、幅度也超大。
  // 这里累加换算成"格数"（1 格 ≈ 120 像素），触控板的小增量也能正确合成。
  let wheelAcc = 0;
  canvas.addEventListener('wheel', (ev) => {
    if (!state.active) return;
    ev.preventDefault();
    let p = ev.deltaY;
    if (ev.deltaMode === 1) p *= 40;       // 行
    else if (ev.deltaMode === 2) p *= 800; // 页
    wheelAcc += p;
    const n = Math.trunc(wheelAcc / 120);
    if (n) {
      wheelAcc -= n * 120;
      sendControl({ t: 'wheel', dy: n });
    }
  }, { passive: false });

  // 窗口失焦时补发松开所有修饰键，避免远端 Shift/Ctrl 卡在按下状态
  window.addEventListener('blur', () => {
    if (!state.active) return;
    for (const k of ['Shift', 'Control', 'Alt', 'Meta']) {
      sendControl({ t: 'key_up', k, mods: [] });
    }
  });

  canvas.addEventListener('contextmenu', (ev) => ev.preventDefault());

  // 焦点在工具栏输入框/下拉框时，键盘不转发到远端
  const inFormField = (ev) => {
    const t = ev.target;
    return t && (t.tagName === 'INPUT' || t.tagName === 'SELECT' || t.tagName === 'TEXTAREA');
  };

  window.addEventListener('keydown', (ev) => {
    if (!state.active) return;
    if (inFormField(ev)) return;
    const k = keyName(ev);
    if (!k) return;
    ev.preventDefault();
    sendControl({ t: 'key_down', k, mods: modsOf(ev, k) });
  });

  window.addEventListener('keyup', (ev) => {
    if (!state.active) return;
    if (inFormField(ev)) return;
    const k = keyName(ev);
    if (!k) return;
    ev.preventDefault();
    sendControl({ t: 'key_up', k, mods: modsOf(ev, k) });
  });
}

/** 组合键修饰状态（键本身是修饰键时排除，避免远端重复按下） */
function modsOf(ev, key) {
  const m = [];
  if (ev.ctrlKey && key !== 'Control') m.push('Control');
  if (ev.shiftKey && key !== 'Shift') m.push('Shift');
  if (ev.altKey && key !== 'Alt') m.push('Alt');
  if (ev.metaKey && key !== 'Meta') m.push('Meta');
  return m;
}

/** KeyboardEvent.key → 协议键名（单字符原样 / 命名键） */
function keyName(ev) {
  const k = ev.key;
  if (!k) return null;
  if (k === ' ') return 'Space';
  if (k === 'Esc') return 'Escape';
  if (/^F([1-9]|1[0-2])$/.test(k)) return k;
  if (k.length === 1) return k;
  const named = [
    'Enter', 'Backspace', 'Tab', 'Escape', 'Space',
    'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight',
    'Home', 'End', 'PageUp', 'PageDown', 'Delete', 'Insert',
    'Shift', 'Control', 'Alt', 'Meta',
  ];
  return named.includes(k) ? k : null;
}

// ============================================================
// 工具栏：显示器 / 画质 / 分辨率 / 刷新 / 截图 / 锁定 / 重启 / 全屏
// ============================================================
async function setFullscreen(on) {
  state.fullscreen = on;
  $('fullscreen-btn').textContent = on ? '退出全屏' : '全屏';
  try {
    await invoke('set_fullscreen', { on });
  } catch (err) {
    webLog('warn', '全屏失败: ' + err);
  }
}

function bindToolbar() {
  $('monitor-select').addEventListener('change', () => {
    const idx = Number($('monitor-select').value);
    if (idx >= 0) {
      webLog('info', `切换显示器 → #${idx}`);
      state.curMonitor = idx;
      sendControl({ t: 'switch_monitor', index: idx });
    }
  });

  $('quality-select').addEventListener('change', async () => {
    let level = $('quality-select').value;
    let kbps = 0;
    if (level === 'custom') {
      const v = prompt('自定义码率（kbps，150 - 8000）', '2000');
      if (!v) {
        $('quality-select').value = 'balanced';
        return;
      }
      kbps = Math.max(150, Math.min(8000, Number(v) || 2000));
    }
    sendControl({
      t: 'set_quality',
      level,
      bitrate_kbps: kbps,
      max_width: Number($('encode-select').value) || 0,
    });
    $('status-text').textContent =
      level === 'custom' ? `码率：自定义 ${kbps} kbps` : `码率：${$('quality-select').selectedOptions[0].textContent}`;
  });

  // 编码画质（实际发出的分辨率上限），不再固定 1920×1080
  $('encode-select').addEventListener('change', () => {
    const maxWidth = Number($('encode-select').value) || 0;
    localStorage.setItem(LS.encode, String(maxWidth));
    sendControl({ t: 'set_quality', level: 'keep', bitrate_kbps: 0, max_width: maxWidth });
    $('status-text').textContent = `编码画质：${$('encode-select').selectedOptions[0].textContent}`;
  });

  // Win 快捷键：WebView 收不到系统 Win 键，提供常用组合的按钮入口
  $('win-select').addEventListener('change', () => {
    const v = $('win-select').value;
    $('win-select').value = '';
    if (!v) return;
    const combo = v.split('+');
    const keys = combo.map((k) => (k === 'Win' ? 'LWin' : k));
    if (combo.length === 1) {
      sendControl({ t: 'key_down', k: keys[0], mods: [] });
      sendControl({ t: 'key_up', k: keys[0], mods: [] });
    } else {
      // 依次按下修饰键和主键；被控端 OS 会保持按键状态，无需重复带 mods
      for (const k of keys) sendControl({ t: 'key_down', k, mods: [] });
      for (const k of keys.slice().reverse()) sendControl({ t: 'key_up', k, mods: [] });
    }
    $('status-text').textContent = `已发送 Win 快捷键：${v}`;
  });

  $('res-select').addEventListener('change', () => {
    const v = $('res-select').value;
    if (!v) return;
    const [w, h] = v.split('x').map(Number);
    if (v === '0x0') {
      sendControl({ t: 'set_resolution', width: 0, height: 0 });
      $('status-text').textContent = '正在恢复被控端原始分辨率…';
    } else {
      sendControl({ t: 'set_resolution', width: w, height: h });
      $('status-text').textContent = `正在将被控端分辨率切换为 ${w}×${h}…`;
    }
    $('res-select').value = '';
  });

  $('refresh-btn').addEventListener('click', () => {
    sendControl({ t: 'refresh' });
    $('status-text').textContent = '已请求刷新画面';
  });

  // 画面卡死且刷新无效时，让被控端 Agent 自愈重启（2 秒后拉起新进程）
  $('heal-btn').addEventListener('click', () => {
    if (!confirm('让被控端 Agent 自愈重启？连接会断开约 5 秒后自动重连。')) return;
    sendControl({ t: 'restart_agent', soft: false, reason: '主控端手动触发' });
    $('status-text').textContent = '已下发自愈重启指令';
  });

  $('add-virtual-btn').addEventListener('click', async () => {
    const count = Number(prompt('新增几块虚拟屏？(1-4)', '1')) || 1;
    const w = Number(prompt('宽度', '1920')) || 1920;
    const h = Number(prompt('高度', '1080')) || 1080;
    sendControl({ t: 'add_virtual_display', count, width: w, height: h });
    $('status-text').textContent = `请求新增 ${count} 块 ${w}×${h} 虚拟屏…`;
  });

  $('remove-virtual-btn').addEventListener('click', () => {
    const idx = Number($('monitor-select').value);
    if (idx < 0) {
      $('status-text').textContent = '请先选择要删除的虚拟屏';
      return;
    }
    const opt = $('monitor-select').selectedOptions[0];
    if (!opt.textContent.includes('💻')) {
      if (!confirm(`当前选中的不是虚拟屏 (${opt.textContent})，确定删除？`)) return;
    }
    sendControl({ t: 'remove_virtual_display', index: idx });
    $('status-text').textContent = `请求删除虚拟屏 #${idx}…`;
  });

  $('clip-btn').addEventListener('click', async () => {
    state.clipOn = !state.clipOn;
    await invoke('set_clipboard_enabled', { on: state.clipOn }).catch(() => {});
    $('clip-btn').classList.toggle('on', state.clipOn);
    $('clip-btn').textContent = state.clipOn ? '剪贴板' : '剪贴板 关';
  });

  $('shot-btn').addEventListener('click', async () => {
    if (!canvas.width || !canvas.height) return;
    try {
      const data = canvas.toDataURL('image/png');
      const path = await invoke('save_screenshot', { data });
      $('file-status').textContent = '截图已保存：' + path;
      setTimeout(() => { $('file-status').textContent = ''; }, 6000);
    } catch (err) {
      $('file-status').textContent = '截图失败：' + String(err);
    }
  });

  $('lock-btn').addEventListener('click', () => {
    if (!confirm('锁定被控端桌面？锁定后需要对方或主控重新登录。')) return;
    sendControl({ t: 'lock' });
    $('status-text').textContent = '已请求锁定被控端';
  });

  $('unlock-btn').addEventListener('click', () => {
    const pwd = prompt('输入被控端 Windows 登录密码（仅通过加密通道传输，不保存）：');
    if (pwd === null) return;
    if (!pwd) { alert('密码不能为空'); return; }
    sendControl({ t: 'unlock', password: pwd });
    $('status-text').textContent = '已发送解锁请求（需要被控端为 SYSTEM 服务）';
  });

  $('restart-btn').addEventListener('click', () => {
    if (!confirm('确定重启被控端电脑？连接会立即断开。')) return;
    sendControl({ t: 'power', action: 'restart', delay_secs: 5 });
    $('status-text').textContent = '已下发重启命令（5 秒后）';
  });

  $('fullscreen-btn').addEventListener('click', () => setFullscreen(!state.fullscreen));

  window.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape' && state.fullscreen) setFullscreen(false);
  });

  $('file-btn').addEventListener('click', () => $('file-input').click());
  $('file-input').addEventListener('change', onFilePicked);

  // 自动更新：connect 时 Rust 侧检查，发现新版本点亮按钮，点击下载安装并重启
  $('update-btn').addEventListener('click', async () => {
    const btn = $('update-btn');
    btn.disabled = true;
    btn.textContent = '正在下载安装…';
    try {
      await invoke('install_update'); // 成功后应用自动重启
    } catch (err) {
      webLog('warn', '更新失败: ' + err);
      btn.textContent = '更新失败';
      setTimeout(() => { btn.disabled = false; btn.hidden = true; }, 4000);
    }
  });
}

const FILE_CHUNK = 48 * 1024;      // DataChannel 文本帧 JSON + base64，块不宜过大
const FILE_MAX = 256 * 1024 * 1024; // 256MB 上限（全量读入内存）

async function onFilePicked() {
  const f = $('file-input').files[0];
  $('file-input').value = '';
  if (!f || !state.active || state.sendingFile) return;
  if (f.size > FILE_MAX) {
    $('file-status').textContent = '文件超过 256MB 上限';
    return;
  }

  state.sendingFile = true;
  $('file-btn').disabled = true;
  const chunks = Math.max(1, Math.ceil(f.size / FILE_CHUNK));
  try {
    sendControl({ t: 'file_offer', name: f.name, size: f.size, chunks });
    const bytes = new Uint8Array(await f.arrayBuffer());
    for (let i = 0; i < chunks; i++) {
      if (!state.active) {
        sendControl({ t: 'file_cancel' });
        $('file-status').textContent = '已取消';
        return;
      }
      const slice = bytes.subarray(i * FILE_CHUNK, Math.min((i + 1) * FILE_CHUNK, bytes.length));
      sendControl({ t: 'file_chunk', seq: i, data: b64encode(slice) });
      $('file-status').textContent = `发送 ${f.name} ${i + 1}/${chunks}`;
      // 让出事件循环，避免 IPC 洪峰把 DataChannel 缓冲打满
      await new Promise((r) => setTimeout(r, 4));
    }
    sendControl({ t: 'file_done' });
    $('file-status').textContent = `已发送 ${f.name}`;
    webLog('info', `文件发送完成 ${f.name} (${f.size}B, ${chunks}块)`);
  } catch (err) {
    webLog('error', '文件发送失败: ' + (err && err.message ? err.message : err));
    sendControl({ t: 'file_cancel' });
    $('file-status').textContent = '发送失败';
  } finally {
    state.sendingFile = false;
    $('file-btn').disabled = false;
    setTimeout(() => { $('file-status').textContent = ''; }, 5000);
  }
}

// ============================================================
// 终端：term DataChannel（sid 多路复用）→ xterm.js
// ============================================================
const termState = {
  ready: false,
  nextSid: 1,
  activeSid: null,
  tabs: new Map(), // sid -> { sid, pane, btn, term, fit, shell, opened, exited }
};

/** 进入会话 / 断开时清空终端视图（服务端进程随会话结束回收） */
function termReset() {
  for (const [, t] of termState.tabs) {
    try { t.term.dispose(); } catch {}
    t.pane.remove();
    t.btn.remove();
  }
  termState.tabs.clear();
  termState.ready = false;
  termState.nextSid = 1;
  termState.activeSid = null;
  $('view-tabs').hidden = true;
  $('term-panes').hidden = true;
  $('desktop-pane').hidden = false;
  document.querySelector('.view-tab[data-view="desktop"]').classList.add('active');
}

/** 切换 桌面 / 指定终端标签 */
function termSelectView(view) {
  const isDesktop = view === 'desktop';
  $('desktop-pane').hidden = !isDesktop;
  $('term-panes').hidden = isDesktop;
  document
    .querySelector('.view-tab[data-view="desktop"]')
    .classList.toggle('active', isDesktop);
  for (const [sid, t] of termState.tabs) {
    const active = !isDesktop && String(sid) === view;
    t.pane.classList.toggle('active', active);
    t.btn.classList.toggle('active', active);
  }
  termState.activeSid = isDesktop ? null : Number(view);
  if (isDesktop) return;
  const t = termState.tabs.get(Number(view));
  if (!t) return;
  requestAnimationFrame(() => {
    try { t.fit.fit(); } catch {}
    if (t.opened) {
      invoke('term_resize', { sid: t.sid, cols: t.term.cols, rows: t.term.rows }).catch(() => {});
    }
    t.term.focus();
  });
}

async function termOpenTab(shell) {
  if (!termState.ready || !state.active) return;
  const sid = termState.nextSid++;
  const pane = document.createElement('div');
  pane.className = 'term-pane';
  $('term-panes').appendChild(pane);

  const term = new Terminal({
    cursorBlink: true,
    fontSize: 13,
    fontFamily: 'Consolas, "Cascadia Mono", "Microsoft YaHei Mono", monospace',
    scrollback: 5000,
    theme: { background: '#0b0e14', foreground: '#d7dce5', cursor: '#7ab8ff' },
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(pane);

  const tab = { sid, pane, term, fit, shell, opened: false, exited: false, btn: null };
  termState.tabs.set(sid, tab);

  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'view-tab';
  btn.dataset.view = String(sid);
  btn.innerHTML = `<span>终端 ${sid}</span><span class="close" title="关闭终端">×</span>`;
  btn.addEventListener('click', (e) => {
    if (e.target.classList.contains('close')) termCloseTab(sid);
    else termSelectView(String(sid));
  });
  $('view-tab-list').appendChild(btn);
  tab.btn = btn;

  term.onData((d) => {
    if (tab.exited) return;
    invoke('term_input', { sid, data: d }).catch(() => {});
  });
  term.onResize(({ cols, rows }) => {
    if (!tab.opened) return;
    invoke('term_resize', { sid, cols, rows }).catch(() => {});
  });

  termSelectView(String(sid));
  // 等一帧让 pane 有尺寸，fit 才有正确的 cols/rows
  await new Promise((r) => requestAnimationFrame(r));
  try { fit.fit(); } catch {}

  try {
    await invoke('term_open', {
      sid,
      cols: term.cols || 80,
      rows: term.rows || 24,
      shell: shell || 'cmd',
      cwd: null,
    });
    tab.opened = true;
  } catch (err) {
    term.write(`\r\n\x1b[31m终端打开失败: ${err}\x1b[0m\r\n`);
    return;
  }
  term.focus();
}

function termCloseTab(sid) {
  const t = termState.tabs.get(sid);
  if (!t) return;
  invoke('term_close', { sid }).catch(() => {});
  try { t.term.dispose(); } catch {}
  t.pane.remove();
  t.btn.remove();
  termState.tabs.delete(sid);
  if (termState.activeSid !== sid) return;
  const rest = [...termState.tabs.keys()];
  termSelectView(rest.length ? String(rest[rest.length - 1]) : 'desktop');
}

/** boot 时绑定终端相关事件 */
async function bindTerm() {
  await listen('term-ready', () => {
    termState.ready = true;
    $('view-tabs').hidden = false;
    webLog('info', '对端支持终端');
  });
  await listen('term-opened', (ev) => {
    const sid = ev.payload && ev.payload.sid;
    const t = termState.tabs.get(sid);
    if (!t) return;
    t.opened = true;
    try { t.fit.fit(); } catch {}
    invoke('term_resize', { sid, cols: t.term.cols, rows: t.term.rows }).catch(() => {});
  });
  await listen('term-data', (ev) => {
    const p = ev.payload || {};
    const t = termState.tabs.get(p.sid);
    if (t) t.term.write(p.data || '');
  });
  await listen('term-exit', (ev) => {
    const p = ev.payload || {};
    const t = termState.tabs.get(p.sid);
    if (!t) return;
    t.exited = true;
    t.term.write(`\r\n\x1b[90m[进程已退出 code=${p.code}]\x1b[0m\r\n`);
  });
  await listen('term-err', (ev) => {
    const p = ev.payload || {};
    const t = termState.tabs.get(p.sid);
    if (t) t.term.write(`\r\n\x1b[31m${p.msg || '终端错误'}\x1b[0m\r\n`);
  });

  $('term-new-btn').addEventListener('click', () => termOpenTab($('term-shell').value));
  document
    .querySelector('.view-tab[data-view="desktop"]')
    .addEventListener('click', () => termSelectView('desktop'));
  window.addEventListener('resize', () => {
    const t = termState.tabs.get(termState.activeSid);
    if (!t) return;
    try { t.fit.fit(); } catch {}
  });
}

function b64encode(bytes) {
  let s = '';
  const STEP = 0x8000;
  for (let i = 0; i < bytes.length; i += STEP) {
    s += String.fromCharCode.apply(null, bytes.subarray(i, i + STEP));
  }
  return btoa(s);
}

boot();
