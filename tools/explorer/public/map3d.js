// 认知地图 · 立体视图(three.js / WebGL)。
//
// 只在用户切到「立体」时才加载(terrain.js 里动态 import),平面图与首页不受影响;
// 浏览器没有 WebGL 时 mount() 返回 null,调用方退回手写的 Canvas 版本。
//
// 读法与平面图一致:地面沿用平面图的布局(列 = 领域,行 = 层级;基础在近处、应用在远处),两种视图对得上号。
//   山高      = 链上算好的地形高度(可信度 × 重要性:被依据、被核验得越多越重要)
//   山体宽窄  = 有多少单元直接或间接依据它
//   山顶的点  = 单元本身,颜色是有效可信度(与平面图同一套色阶),越可信越亮
//   弧线      = 引用:实线依据、虚线参见、红线取代;依据线上的光点表示可信度传递的方向
// three.js 以 MIT 许可随网页一起分发(public/vendor/three/),不走外部 CDN。
import * as T from "./vendor/three/three-map.min.js";

const BG = 0x0d1117;
// 地形按高度着色:暗底上的冷灰蓝,和点的蓝色色阶拉开,不抢可信度的颜色
const TERRAIN_STOPS = [[0, "#18202b"], [0.18, "#223042"], [0.45, "#34475e"], [0.75, "#566e8c"], [1, "#8aa2c0"]];

export function mount(host, data, onPick, h) {
  const reduce = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const size = () => {
    const w = Math.max(640, host.clientWidth);
    return [w, Math.min(640, Math.max(440, Math.round(w * 0.56)))];
  };
  let [W, H] = size();

  let renderer;
  try {
    renderer = new T.WebGLRenderer({ antialias: true, powerPreference: "high-performance" });
  } catch (_) {
    return null;                                   // 没有 WebGL
  }
  if (!renderer.getContext()) return null;
  renderer.setPixelRatio(Math.min(2, window.devicePixelRatio || 1));
  renderer.setSize(W, H);
  renderer.outputColorSpace = T.SRGBColorSpace;
  renderer.toneMapping = T.ACESFilmicToneMapping;
  renderer.toneMappingExposure = 1.35;
  renderer.shadowMap.enabled = true;
  renderer.shadowMap.type = T.PCFSoftShadowMap;

  host.querySelectorAll(":scope > svg, :scope > canvas").forEach((x) => x.remove());
  const cv = renderer.domElement;
  Object.assign(cv.style, { display: "block", touchAction: "none", cursor: "grab" });
  cv.tabIndex = 0;
  cv.setAttribute("role", "img");
  cv.setAttribute("aria-label", "认知地图立体视图:山高是可信度乘重要性,山体宽窄是依据它的单元数,弧线是引用。拖动旋转,滚轮缩放,方向键也能转;下方有表格视图。");
  host.insertBefore(cv, host.firstChild);

  // 文字用 HTML 叠在画面上:中文和平面图一样清楚,缩放也不发糊
  const css = new T.CSS2DRenderer();
  css.setSize(W, H);
  Object.assign(css.domElement.style, { position: "absolute", left: "0", top: "0", pointerEvents: "none", zIndex: "1" });
  host.appendChild(css.domElement);

  const scene = new T.Scene();
  scene.background = new T.Color(BG);

  // ---- 地形 -------------------------------------------------------------------
  const L = h.layout(data, 1100);
  const nodes = data.nodes;
  const ax = nodes.map((n) => n._x), az = nodes.map((n) => n._y);
  const cX = (Math.min(...ax) + Math.max(...ax)) / 2, cZ = (Math.min(...az) + Math.max(...az)) / 2;
  const peaks = nodes.map((n) => ({
    n, x: n._x - cX, z: n._y - cZ,
    h: 10 + (n.height != null ? n.height : 0) * 185,
    s: 30 + Math.sqrt(n.downstream || 0) * 13,
  }));
  const PAD = 120;
  const x0 = Math.min(...peaks.map((p) => p.x)) - PAD, x1 = Math.max(...peaks.map((p) => p.x)) + PAD;
  const z0 = Math.min(...peaks.map((p) => p.z)) - PAD, z1 = Math.max(...peaks.map((p) => p.z)) + PAD;

  // 确定性的细碎起伏(值噪声),让平地不死板;每次打开形状都一样
  const hash = (i, j) => { const s = Math.sin(i * 127.1 + j * 311.7) * 43758.5453; return s - Math.floor(s); };
  const smooth = (t) => t * t * (3 - 2 * t);
  const vnoise = (x, z) => {
    const i = Math.floor(x), j = Math.floor(z), fx = smooth(x - i), fz = smooth(z - j);
    const a = hash(i, j), b = hash(i + 1, j), c = hash(i, j + 1), d = hash(i + 1, j + 1);
    return a + (b - a) * fx + (c - a) * fz + (a - b - c + d) * fx * fz;
  };
  const fbm = (x, z) => vnoise(x / 70, z / 70) * 0.6 + vnoise(x / 31, z / 31) * 0.3 + vnoise(x / 13, z / 13) * 0.1;
  const edgeFade = (x, z) => {
    const d = Math.min(x - x0, x1 - x, z - z0, z1 - z);
    const t = Math.max(0, Math.min(1, d / 80));
    return smooth(t);
  };
  // 每座山一个高斯;用"软最大值"合在一起,山与山之间是平滑的鞍部而不是折痕
  const field = (x, z) => {
    let acc = 0;
    for (const p of peaks) {
      const g = p.h * Math.exp(-((x - p.x) ** 2 + (z - p.z) ** 2) / (2 * p.s * p.s));
      acc += g * g * g * g;
    }
    return (Math.pow(acc, 0.25) + fbm(x, z) * 7) * edgeFade(x, z);
  };

  const NX = 220, NZ = Math.max(60, Math.round(NX * (z1 - z0) / (x1 - x0)));
  const dx = (x1 - x0) / NX, dz = (z1 - z0) / NZ;
  const V = new Float32Array((NX + 1) * (NZ + 1));
  const pos = new Float32Array(V.length * 3);
  let hMax = 1;
  for (let k = 0; k <= NZ; k++) for (let i = 0; i <= NX; i++) {
    const x = x0 + i * dx, z = z0 + k * dz, y = field(x, z), o = k * (NX + 1) + i;
    V[o] = y; pos[o * 3] = x; pos[o * 3 + 1] = y; pos[o * 3 + 2] = z;
    if (y > hMax) hMax = y;
  }
  const idx = [];
  for (let k = 0; k < NZ; k++) for (let i = 0; i < NX; i++) {
    const a = k * (NX + 1) + i, b = a + 1, c = a + NX + 1, d = c + 1;
    idx.push(a, c, b, b, c, d);
  }
  const stops = TERRAIN_STOPS.map(([t, hex]) => [t, new T.Color(hex)]);
  const col = new Float32Array(V.length * 3), tmp = new T.Color();
  for (let o = 0; o < V.length; o++) {
    const t = Math.min(1, V[o] / hMax);
    let s = 0; while (s < stops.length - 2 && t > stops[s + 1][0]) s++;
    const [ta, ca] = stops[s], [tb, cb] = stops[s + 1];
    tmp.copy(ca).lerp(cb, (t - ta) / (tb - ta || 1));
    col[o * 3] = tmp.r; col[o * 3 + 1] = tmp.g; col[o * 3 + 2] = tmp.b;
  }
  const tGeo = new T.BufferGeometry();
  tGeo.setAttribute("position", new T.BufferAttribute(pos, 3));
  tGeo.setAttribute("color", new T.BufferAttribute(col, 3));
  tGeo.setIndex(idx);
  tGeo.computeVertexNormals();
  const terrain = new T.Mesh(tGeo, new T.MeshStandardMaterial({ vertexColors: true, roughness: 0.93, metalness: 0 }));
  terrain.receiveShadow = true; terrain.castShadow = true;
  scene.add(terrain);

  // 底座:把地形托成一块"沙盘"
  const slab = new T.Mesh(new T.BoxGeometry(x1 - x0, 28, z1 - z0),
    new T.MeshStandardMaterial({ color: 0x10151c, roughness: 1, polygonOffset: true, polygonOffsetFactor: 2, polygonOffsetUnits: 2 }));
  slab.position.set((x0 + x1) / 2, -14, (z0 + z1) / 2);
  slab.receiveShadow = true;
  scene.add(slab);

  // 等高线(marching squares):看得出山势,不必靠颜色猜高度
  const contour = [];
  const step = hMax / 9;
  for (let lv = step; lv < hMax; lv += step) {
    for (let k = 0; k < NZ; k++) for (let i = 0; i < NX; i++) {
      const o = k * (NX + 1) + i;
      const v0 = V[o], v1 = V[o + 1], v2 = V[o + NX + 2], v3 = V[o + NX + 1];
      const c = (v0 > lv ? 1 : 0) | (v1 > lv ? 2 : 0) | (v2 > lv ? 4 : 0) | (v3 > lv ? 8 : 0);
      if (c === 0 || c === 15) continue;
      const X = x0 + i * dx, Z = z0 + k * dz, y = lv + 0.7;
      const e = [
        [X + dx * (lv - v0) / (v1 - v0), Z],             // 下边 v0-v1
        [X + dx, Z + dz * (lv - v1) / (v2 - v1)],        // 右边 v1-v2
        [X + dx * (lv - v3) / (v2 - v3), Z + dz],        // 上边 v3-v2
        [X, Z + dz * (lv - v0) / (v3 - v0)],             // 左边 v0-v3
      ];
      const segs = { 1: [[3, 0]], 2: [[0, 1]], 3: [[3, 1]], 4: [[1, 2]], 5: [[3, 0], [1, 2]], 6: [[0, 2]], 7: [[3, 2]],
        8: [[2, 3]], 9: [[0, 2]], 10: [[0, 1], [2, 3]], 11: [[1, 2]], 12: [[1, 3]], 13: [[0, 1]], 14: [[0, 3]] }[c];
      for (const [p, q] of segs) contour.push(e[p][0], y, e[p][1], e[q][0], y, e[q][1]);
    }
  }
  const cGeo = new T.BufferGeometry();
  cGeo.setAttribute("position", new T.Float32BufferAttribute(contour, 3));
  scene.add(new T.LineSegments(cGeo, new T.LineBasicMaterial({ color: 0x9db3cf, transparent: true, opacity: 0.16, depthWrite: false })));

  // 地面分区线:领域之间、层级之间(贴着地形画,与平面图的分隔对应)
  const drape = (pts) => {
    const out = [];
    for (let s = 0; s < pts.length - 1; s++) {
      const [ax, az] = pts[s], [bx, bz] = pts[s + 1];
      const n = Math.max(2, Math.ceil(Math.hypot(bx - ax, bz - az) / 6));
      for (let t = 0; t <= n; t++) { const x = ax + (bx - ax) * t / n, z = az + (bz - az) * t / n; out.push(new T.Vector3(x, field(x, z) + 1.2, z)); }
    }
    const g = new T.BufferGeometry().setFromPoints(out);
    const line = new T.Line(g, new T.LineDashedMaterial({ color: 0x8b9bb0, dashSize: 5, gapSize: 6, transparent: true, opacity: 0.4, depthWrite: false }));
    line.computeLineDistances();
    scene.add(line);
  };
  for (let i = 1; i < L.domains.length; i++) {
    const x = L.padL + i * (L.colW + L.colGap) - L.colGap / 2 - cX;
    drape([[x, z0 + 30], [x, z1 - 30]]);
  }
  L.layers.forEach((l, i) => {
    if (i === L.layers.length - 1) return;           // 最上一层的上边界就是地图边缘
    const z = L.rowTop.get(l) - cZ;
    drape([[x0 + 30, z], [x1 - 30, z]]);
  });

  // ---- 灯光 -------------------------------------------------------------------
  scene.add(new T.HemisphereLight(0xc4d2e6, 0x1a2230, 1.1));
  const sun = new T.DirectionalLight(0xfff4e6, 2.1);
  sun.position.set(-520, 760, 420);
  sun.castShadow = true;
  sun.shadow.mapSize.set(2048, 2048);
  const half = Math.max(x1 - x0, z1 - z0) / 2 + 120;
  Object.assign(sun.shadow.camera, { left: -half, right: half, top: half, bottom: -half, near: 100, far: 2600 });
  sun.shadow.bias = -0.0004; sun.shadow.normalBias = 1.5;
  scene.add(sun);
  const rim = new T.DirectionalLight(0x3987e5, 0.6);      // 背后一道冷色轮廓光,让山脊从暗底里浮出来
  rim.position.set(380, 260, -700);
  scene.add(rim);

  // ---- 单元(山顶的点)----------------------------------------------------------
  const glowTex = (() => {
    const c = document.createElement("canvas"); c.width = c.height = 64;
    const g = c.getContext("2d"), grd = g.createRadialGradient(32, 32, 0, 32, 32, 32);
    grd.addColorStop(0, "rgba(255,255,255,1)"); grd.addColorStop(0.35, "rgba(255,255,255,.35)"); grd.addColorStop(1, "rgba(255,255,255,0)");
    g.fillStyle = grd; g.fillRect(0, 0, 64, 64);
    const t = new T.CanvasTexture(c); t.colorSpace = T.SRGBColorSpace; return t;
  })();
  const sphere = new T.SphereGeometry(1, 32, 18);
  const stemGeo = new T.CylinderGeometry(0.55, 0.55, 1, 6);
  const hitMat = new T.MeshBasicMaterial({ visible: false });
  const NODE = new Map();
  const hits = [];
  for (const p of peaks) {
    const n = p.n, ground = field(p.x, p.z), r = 7 + Math.min(12, Math.sqrt(n.downstream || 0) * 3.5);
    const y = ground + r + 16;
    const color = new T.Color(h.credColor(n.effective));
    const mat = new T.MeshStandardMaterial({ color, emissive: color, emissiveIntensity: 0.45 + 0.9 * (n.effective || 0),
      roughness: 0.32, metalness: 0.15, transparent: true });
    const ball = new T.Mesh(sphere, mat);
    ball.scale.setScalar(r); ball.position.set(p.x, y, p.z);
    scene.add(ball);
    const glow = new T.Sprite(new T.SpriteMaterial({ map: glowTex, color, transparent: true, depthWrite: false,
      blending: T.AdditiveBlending, opacity: 0.35 + 0.5 * (n.effective || 0) }));
    glow.scale.setScalar(r * 4.2); glow.position.copy(ball.position);
    scene.add(glow);
    const stemMat = new T.MeshBasicMaterial({ color: 0xb9c3d2, transparent: true, opacity: 0.55 });
    const stem = new T.Mesh(stemGeo, stemMat);
    stem.scale.set(1, y - r - ground + 2, 1); stem.position.set(p.x, (ground - 2 + y - r) / 2, p.z);
    scene.add(stem);
    const hit = new T.Mesh(sphere, hitMat);          // 命中区比点大,好点
    hit.scale.setScalar(r + 8); hit.position.copy(ball.position); hit.userData.id = n.id;
    scene.add(hit); hits.push(hit);
    // 标题
    const div = document.createElement("div");
    div.className = "m3Lbl";
    const span = document.createElement("span");
    span.textContent = h.clip(`#${n.id} ${h.shortTitle(n.title)}`.trim(), 11);
    div.appendChild(span);
    const label = new T.CSS2DObject(div);
    label.position.set(p.x, y + r + 9, p.z);
    scene.add(label);
    NODE.set(n.id, { n, ball, mat, glow, stemMat, label, span, pos: ball.position, w: 0 });
  }

  // ---- 引用(山顶之间的弧线)-----------------------------------------------------
  const EDGES = [];
  const pulses = [];
  (data.edges || []).forEach((e, ix) => {
    const a = NODE.get(e.from), b = NODE.get(e.to);
    if (!a || !b) return;
    const mid = a.pos.clone().lerp(b.pos, 0.5);
    mid.y += 30 + a.pos.distanceTo(b.pos) * 0.28;
    const curve = new T.QuadraticBezierCurve3(a.pos.clone(), mid, b.pos.clone());
    let obj, mat, base;
    if (e.kind === "see-also") {
      mat = new T.LineDashedMaterial({ color: 0x7d8896, dashSize: 4, gapSize: 5, transparent: true, opacity: base = 0.55 });
      obj = new T.Line(new T.BufferGeometry().setFromPoints(curve.getPoints(48)), mat);
      obj.computeLineDistances();
    } else {
      const red = e.kind === "supersedes";
      mat = new T.MeshBasicMaterial({ color: red ? 0xf85149 : 0x93a7c2, transparent: true, opacity: base = red ? 0.9 : 0.7 });
      obj = new T.Mesh(new T.TubeGeometry(curve, 48, red ? 1.1 : 0.85, 6), mat);
    }
    scene.add(obj);
    EDGES.push({ e, mat, base });
    // 依据线上的光点:从被依据者流向依据者(可信度沿依据传递的方向)
    if (e.kind === "basis" && !reduce) {
      const back = new T.QuadraticBezierCurve3(b.pos.clone(), mid, a.pos.clone());
      const dot = new T.Mesh(sphere, new T.MeshBasicMaterial({ color: 0xdbe7f7, transparent: true, opacity: 0.95 }));
      dot.scale.setScalar(1.9);
      scene.add(dot);
      pulses.push({ e, dot, curve: back, phase: (ix * 0.37) % 1 });
    }
  });

  // ---- 轴上的字:层级在左边、领域在近处 ------------------------------------------
  const axisLabel = (text, cls, x, y, z) => {
    const d = document.createElement("div"); d.className = cls; d.textContent = text;
    const o = new T.CSS2DObject(d); o.position.set(x, y, z); scene.add(o);
  };
  L.layers.forEach((l, i) => axisLabel(h.LAYER_NAME[l], "m3Axis", x0 - 26, 0, L.rowTop.get(l) + L.rowH[i] / 2 - cZ));
  L.domains.forEach((d, i) => axisLabel(h.domainLabel(d), "m3Dom", L.padL + i * (L.colW + L.colGap) + L.colW / 2 - cX, -10, z1 + 34));

  // ---- 相机与操作 ----------------------------------------------------------------
  const camera = new T.PerspectiveCamera(36, W / H, 10, 8000);
  const target = new T.Vector3(0, hMax * 0.25, 0);
  const fitDistance = () => {
    const vfov = T.MathUtils.degToRad(camera.fov), hfov = 2 * Math.atan(Math.tan(vfov / 2) * camera.aspect);
    return Math.max(((x1 - x0) / 2) / Math.tan(hfov / 2), ((z1 - z0) / 2 + hMax) / Math.tan(vfov / 2)) * 1.02;
  };
  // 取景:按实际内容(沙盘四角 + 各山顶)投影来定距离与中心,不留大片空白
  const box = [];
  for (const x of [x0, x1]) for (const z of [z0, z1]) box.push(new T.Vector3(x, 0, z));
  for (const o of NODE.values()) box.push(o.pos.clone().setY(o.pos.y + 30));
  for (const e of data.edges || []) {                 // 弧线的最高点也要装进画面
    const a = NODE.get(e.from), c = NODE.get(e.to);
    if (!a || !c) continue;
    const m = a.pos.clone().lerp(c.pos, 0.5);
    m.y += (30 + a.pos.distanceTo(c.pos) * 0.28) / 2 + 12;   // 二次贝塞尔的顶点在控制点高度的一半
    box.push(m);
  }
  const v = new T.Vector3();
  const HOME_PHI = 0.92, HOME_THETA = -0.32;
  const place = (R) => {
    camera.position.copy(target).add(new T.Vector3().setFromSpherical(new T.Spherical(R, HOME_PHI, HOME_THETA)));
    camera.lookAt(target); camera.updateMatrixWorld();
  };
  const home = () => {
    let R = fitDistance();
    const tv = Math.tan(T.MathUtils.degToRad(camera.fov) / 2);
    for (let it = 0; it < 8; it++) {
      place(R);
      let mnx = 1e9, mxx = -1e9, mny = 1e9, mxy = -1e9;
      for (const p of box) { v.copy(p).project(camera); mnx = Math.min(mnx, v.x); mxx = Math.max(mxx, v.x); mny = Math.min(mny, v.y); mxy = Math.max(mxy, v.y); }
      if (![mnx, mxx, mny, mxy].every(Number.isFinite)) break;
      // 中心:屏幕上内容框的中心挪到画面中心(NDC 偏移 × 目标处半个画面的世界尺寸),每次只挪一半,稳
      const cx = (mnx + mxx) / 2, cy = (mny + mxy) / 2;
      const right = new T.Vector3(1, 0, 0).applyQuaternion(camera.quaternion);
      const up = new T.Vector3(0, 1, 0).applyQuaternion(camera.quaternion);
      target.addScaledVector(right, cx * R * tv * camera.aspect * 0.5).addScaledVector(up, cy * R * tv * 0.5);
      // 距离:内容框铺满画面的 92%
      const span = Math.max((mxx - mnx) / 2, (mxy - mny) / 2);
      R *= Math.max(0.6, Math.min(1.6, span / 0.92));
    }
    place(R);
    return R;
  };
  const R0 = home();
  scene.fog = new T.Fog(BG, R0 * 1.05, R0 * 2.6);   // 远处渐隐,前后关系一眼可见

  const controls = new T.OrbitControls(camera, cv);
  controls.target.copy(target);
  Object.assign(controls, {
    enableDamping: true, dampingFactor: 0.08, rotateSpeed: 0.55, zoomSpeed: 0.8, enablePan: false,
    minPolarAngle: 0.18, maxPolarAngle: 1.36, minDistance: R0 * 0.3, maxDistance: R0 * 2,
    autoRotate: !reduce, autoRotateSpeed: 0.55,
  });
  controls.update();
  controls.saveState();

  // ---- 悬停、点击、键盘 -----------------------------------------------------------
  const ray = new T.Raycaster(), ndc = new T.Vector2();
  const pickAt = (ev) => {
    const b = cv.getBoundingClientRect();
    ndc.set(((ev.clientX - b.left) / b.width) * 2 - 1, -((ev.clientY - b.top) / b.height) * 2 + 1);
    ray.setFromCamera(ndc, camera);
    const got = ray.intersectObjects(hits, false)[0];
    return got ? NODE.get(got.object.userData.id).n : null;
  };
  let hover = null, dirty = true;
  function highlight(id) {
    hover = id;
    const lit = new Set(id ? [id] : []);
    if (id) for (const { e } of EDGES) if (e.from === id || e.to === id) { lit.add(e.from); lit.add(e.to); }
    for (const [nid, o] of NODE) {
      const keep = !id || lit.has(nid);
      o.mat.opacity = keep ? 1 : 0.22;
      o.glow.material.opacity = keep ? 0.35 + 0.5 * (o.n.effective || 0) : 0.05;
      o.stemMat.opacity = keep ? 0.55 : 0.12;
      o.span.style.opacity = keep ? "1" : "0.4";
      o.mat.emissiveIntensity = (0.45 + 0.9 * (o.n.effective || 0)) * (nid === id ? 1.8 : 1);
    }
    for (const x of EDGES) {
      const on = id && (x.e.from === id || x.e.to === id);
      x.mat.opacity = !id ? x.base : on ? 1 : 0.05;
    }
    for (const p of pulses) p.dot.visible = !id || p.e.from === id || p.e.to === id;
    dirty = true;
  }
  let down = null;
  cv.addEventListener("pointerdown", (ev) => { down = { x: ev.clientX, y: ev.clientY }; cv.style.cursor = "grabbing"; });
  cv.addEventListener("pointermove", (ev) => {
    if (down) { h.showTip(host, null); return; }
    const n = pickAt(ev), id = n ? n.id : null;
    if (id !== hover) highlight(id);
    cv.style.cursor = n ? "pointer" : "grab";
    h.showTip(host, n ? ev : null, n);
  });
  cv.addEventListener("pointerup", (ev) => {
    const click = down && Math.hypot(ev.clientX - down.x, ev.clientY - down.y) < 5;
    down = null; cv.style.cursor = "grab";
    if (click) { const n = pickAt(ev); if (n) onPick(n); }
  });
  cv.addEventListener("pointercancel", () => { down = null; });
  cv.addEventListener("pointerleave", () => { if (!down) { if (hover) highlight(null); h.showTip(host, null); } });
  controls.addEventListener("start", () => { controls.autoRotate = false; onAuto && onAuto(false); });
  controls.addEventListener("change", () => { dirty = true; });
  const orbit = (dTheta, dPhi) => {
    const off = camera.position.clone().sub(controls.target);
    const s = new T.Spherical().setFromVector3(off);
    s.theta += dTheta; s.phi = Math.max(controls.minPolarAngle, Math.min(controls.maxPolarAngle, s.phi + dPhi));
    camera.position.copy(controls.target).add(off.setFromSpherical(s));
    camera.lookAt(controls.target);
    dirty = true;
  };
  cv.addEventListener("keydown", (ev) => {
    const k = { ArrowLeft: [0.12, 0], ArrowRight: [-0.12, 0], ArrowUp: [0, -0.08], ArrowDown: [0, 0.08] }[ev.key];
    if (!k) return;
    ev.preventDefault();
    controls.autoRotate = false; onAuto && onAuto(false);
    orbit(k[0], k[1]);
  });
  let onAuto = null;

  // 标题挤在一起时:悬停相关的优先,其次离相机近的;放不下的先藏起来,悬停时会出现
  const scr = new T.Vector3();
  function cullLabels() {
    const rows = [];
    for (const o of NODE.values()) {
      if (!o.w) o.w = o.span.offsetWidth;
      scr.copy(o.label.position).project(camera);
      if (scr.z > 1) { o.span.style.visibility = "hidden"; continue; }
      const x = (scr.x + 1) / 2 * W, y = (1 - scr.y) / 2 * H;
      const must = hover && (o.n.id === hover || EDGES.some(({ e }) => (e.from === hover && e.to === o.n.id) || (e.to === hover && e.from === o.n.id)));
      rows.push({ o, must, d: camera.position.distanceTo(o.pos), box: { x0: x - o.w / 2 - 3, x1: x + o.w / 2 + 3, y0: y - 9, y1: y + 9 } });
    }
    rows.sort((a, b) => (b.must - a.must) || (a.d - b.d));
    const placed = [];
    for (const r of rows) {
      const clash = placed.some((q) => r.box.x0 < q.x1 && r.box.x1 > q.x0 && r.box.y0 < q.y1 && r.box.y1 > q.y0);
      const show = !clash || r.must;
      r.o.span.style.visibility = show ? "visible" : "hidden";
      if (show) placed.push(r.box);
    }
  }

  // ---- 渲染循环:画面不动时不重画 --------------------------------------------------
  let alive = true, raf = 0, onScreen = true;
  const io = "IntersectionObserver" in window ? new IntersectionObserver(([en]) => { onScreen = en.isIntersecting; if (onScreen) dirty = true; }) : null;
  io && io.observe(cv);
  const clock = new T.Clock();
  function tick() {
    if (!alive) return;
    if (!document.body.contains(cv)) { stop(); return; }
    raf = requestAnimationFrame(tick);
    if (!onScreen) return;
    const moved = controls.update();
    const t = clock.getElapsedTime();
    for (const p of pulses) { if (p.dot.visible) p.dot.position.copy(p.curve.getPoint((t / 2.8 + p.phase) % 1)); }
    if (moved || dirty || pulses.length) {
      renderer.render(scene, camera);
      css.render(scene, camera);
      cullLabels();
      dirty = false;
    }
  }
  raf = requestAnimationFrame(tick);

  function stop() {
    if (!alive) return;
    alive = false;
    cancelAnimationFrame(raf);
    io && io.disconnect();
    controls.dispose();
    scene.traverse((o) => {
      if (o.geometry) o.geometry.dispose();
      const m = o.material;
      if (m) (Array.isArray(m) ? m : [m]).forEach((x) => { x.map && x.map.dispose(); x.dispose(); });
    });
    renderer.dispose();
    renderer.forceContextLoss();
    cv.remove(); css.domElement.remove();
  }

  return {
    reset() { controls.reset(); dirty = true; },
    toggleAuto() { controls.autoRotate = !controls.autoRotate; return controls.autoRotate; },
    get auto() { return controls.autoRotate; },
    set onAuto(fn) { onAuto = fn; },
    resize() {
      [W, H] = size();
      renderer.setSize(W, H); css.setSize(W, H);
      camera.aspect = W / H; camera.updateProjectionMatrix();
      dirty = true;
    },
    stop,
  };
}
