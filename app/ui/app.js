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
};

// ============================================================
// 输入记忆（断开/重启后仍在，方便再次连接）
// ============================================================
const LS = {
  peer: 'rebleach.peer',
  fps: 'rebleach.fps',
  ice: 'rebleach.ice',
  lanIp: 'rebleach.lanIp',
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

  // 被控端推送的显示器清单 → 填充切换下拉
  await listen('monitors', (ev) => {
    const list = ev.payload || [];
    webLog('info', `[monitors] ${list.length} 台显示器`);
    const sel = $('monitor-select');
    sel.innerHTML = '';
    for (const m of list) {
      const opt = document.createElement('option');
      opt.value = m.index;
      const virtTag = m.is_virtual ? ' 💻' : '';
      opt.textContent = `#${m.index} ${m.name || ''} ${m.width}×${m.height}${m.primary ? '（主）' : ''}${virtTag}`;
      sel.appendChild(opt);
    }
    sel.disabled = !list.length;
  });

  // 被控端链路上报：编码 fps / 发送码率 / DataChannel 缓冲
  await listen('netstats', (ev) => {
    const p = ev.payload || {};
    $('netstats').textContent =
      `远端 ${Math.round(p.fps || 0)} fps · ${Math.round(p.kbps || 0)} kbps · 缓冲 ${Math.round((p.buffered || 0) / 1024)} KB`;
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
  startFpsMeter();
  initCtl();
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
  $('file-status').textContent = '';

  $('toolbar').hidden = false;
  const sel = $('monitor-select');
  sel.innerHTML = '<option value="-1">等待清单…</option>';
  sel.disabled = true;
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
  $('file-status').textContent = '';
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

function bindInput() {
  canvas.addEventListener('mousemove', (ev) => {
    if (!state.active) return;
    const now = performance.now();
    if (now - state.lastMove < 16) return; // ~60Hz 上限
    const p = remotePoint(ev);
    if (!p) return;
    state.lastMove = now;
    sendControl({ t: 'mouse_move', x: p.x, y: p.y });
  });

  canvas.addEventListener('mousedown', (ev) => {
    if (!state.active) return;
    ev.preventDefault();
    canvas.focus();
    sendControl({ t: 'mouse_down', b: BUTTON_MAP[ev.button] ?? 0 });
  });

  window.addEventListener('mouseup', (ev) => {
    if (!state.active) return;
    // 在窗口任意位置释放都补发，避免远端按键卡住
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
    sendControl({ t: 'set_quality', level, bitrate_kbps: kbps });
    $('status-text').textContent =
      level === 'custom' ? `画质：自定义 ${kbps} kbps` : `画质：${$('quality-select').selectedOptions[0].textContent}`;
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

function b64encode(bytes) {
  let s = '';
  const STEP = 0x8000;
  for (let i = 0; i < bytes.length; i += STEP) {
    s += String.fromCharCode.apply(null, bytes.subarray(i, i + STEP));
  }
  return btoa(s);
}

boot();
