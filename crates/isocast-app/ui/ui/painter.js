// Draws a monitor's pictures (WebCodecs VideoFrames, or ImageBitmaps from
// JPEG frames) into its canvas with WebGL: the frame is handed to the GPU as
// a texture and drawn once at its own size, and the page's compositor
// scales the canvas to fit. Drawing VideoFrames on a 2D canvas instead
// converted and scaled them on the CPU (WebKit's GPU process at ~16% of a
// core for one 1664 px monitor at 30 fps, against ~3% this way). Falls back
// to a 2D canvas where WebGL isn't available.

const VERTEX = `#version 300 es
out vec2 uv;
void main() {
  // One triangle, twice the canvas's size, covering it; the texture's
  // first row is the picture's top.
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  uv = vec2(p.x, 1.0 - p.y);
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

const FRAGMENT = `#version 300 es
precision mediump float;
in vec2 uv;
uniform sampler2D picture;
out vec4 color;
void main() { color = texture(picture, uv); }`;

const painters = new WeakMap();

/** Draw `image` (a VideoFrame or an ImageBitmap) into `canvas`. */
export function paint(canvas, image) {
  let painter = painters.get(canvas);
  if (!painter) {
    painter = glPainter(canvas) ?? flatPainter(canvas);
    painters.set(canvas, painter);
  }
  const width = image.displayWidth ?? image.width;
  const height = image.displayHeight ?? image.height;
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }
  painter(image, width, height);
}

function glPainter(canvas) {
  const gl = canvas.getContext("webgl2", {
    alpha: false,
    antialias: false,
    depth: false,
    stencil: false,
    premultipliedAlpha: false,
    preserveDrawingBuffer: false,
  });
  if (!gl) return null;
  let ready = false;
  const setUp = () => {
    const shader = (type, source) => {
      const s = gl.createShader(type);
      gl.shaderSource(s, source);
      gl.compileShader(s);
      return s;
    };
    const program = gl.createProgram();
    gl.attachShader(program, shader(gl.VERTEX_SHADER, VERTEX));
    gl.attachShader(program, shader(gl.FRAGMENT_SHADER, FRAGMENT));
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) return false;
    gl.useProgram(program);
    gl.bindTexture(gl.TEXTURE_2D, gl.createTexture());
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    return true;
  };
  if (!setUp()) return null;
  ready = true;
  // A lost context (the GPU was reset) is set up again once it's back.
  canvas.addEventListener("webglcontextlost", (event) => {
    event.preventDefault();
    ready = false;
  });
  canvas.addEventListener("webglcontextrestored", () => {
    ready = setUp();
  });
  return (image, width, height) => {
    if (!ready) return;
    gl.viewport(0, 0, width, height);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, image);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  };
}

function flatPainter(canvas) {
  const context = canvas.getContext("2d");
  return (image) => context.drawImage(image, 0, 0);
}
