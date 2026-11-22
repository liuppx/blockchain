// 认知地图立体视图用到的 three.js 部件(只打包这些,其余由 esbuild 摇掉)。
// 改 public/map3d.js 用到了新的类时,在这里补上,再运行 scripts/build-three.sh。
export {
  ACESFilmicToneMapping, AdditiveBlending, BoxGeometry, BufferAttribute, BufferGeometry, CanvasTexture, Clock, Color,
  CylinderGeometry, DirectionalLight, Float32BufferAttribute, Fog, HemisphereLight, Line, LineBasicMaterial,
  LineDashedMaterial, LineSegments, MathUtils, Mesh, MeshBasicMaterial, MeshStandardMaterial, PCFSoftShadowMap,
  PerspectiveCamera, QuadraticBezierCurve3, Raycaster, Scene, SphereGeometry, Spherical, Sprite, SpriteMaterial,
  SRGBColorSpace, TubeGeometry, Vector2, Vector3, WebGLRenderer,
} from "three";
export { OrbitControls } from "three/addons/controls/OrbitControls.js";
export { CSS2DRenderer, CSS2DObject } from "three/addons/renderers/CSS2DRenderer.js";
