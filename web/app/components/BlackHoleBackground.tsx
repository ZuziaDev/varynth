"use client";

import React, { useEffect, useRef, useState } from "react";
import * as THREE from "three";

export interface VarynthThemeSpec {
  id: string;
  label: string;
  primaryHex: string;
  secondaryHex: string;
  hotHex: string;
  cssAccent: string;
  cssAccentHover: string;
  cssAccentSoft: string;
  cssCool: string;
  cssCoolSoft: string;
}

export const VARYNTH_THEMES: VarynthThemeSpec[] = [
  {
    id: "terracotta",
    label: "Gargantua Gold",
    primaryHex: "#f59e0b",
    secondaryHex: "#9a3412",
    hotHex: "#fffbeb",
    cssAccent: "oklch(75% 0.155 55)",
    cssAccentHover: "oklch(80% 0.15 60)",
    cssAccentSoft: "oklch(24% 0.05 55 / 0.75)",
    cssCool: "oklch(80% 0.12 75)",
    cssCoolSoft: "oklch(24% 0.045 75 / 0.75)",
  },
  {
    id: "magenta",
    label: "Singularity Violet",
    primaryHex: "#c084fc",
    secondaryHex: "#4c1d95",
    hotHex: "#fdf4ff",
    cssAccent: "oklch(72% 0.19 312)",
    cssAccentHover: "oklch(78% 0.18 312)",
    cssAccentSoft: "oklch(24% 0.06 312 / 0.75)",
    cssCool: "oklch(76% 0.14 275)",
    cssCoolSoft: "oklch(23.5% 0.05 275 / 0.75)",
  },
  {
    id: "cyber",
    label: "Relativistic Cyan",
    primaryHex: "#38bdf8",
    secondaryHex: "#1e3a8a",
    hotHex: "#f0f9ff",
    cssAccent: "oklch(76% 0.14 230)",
    cssAccentHover: "oklch(81% 0.13 225)",
    cssAccentSoft: "oklch(24% 0.05 230 / 0.75)",
    cssCool: "oklch(74% 0.14 255)",
    cssCoolSoft: "oklch(23.5% 0.05 255 / 0.75)",
  },
  {
    id: "emerald",
    label: "Quantum Horizon",
    primaryHex: "#34d399",
    secondaryHex: "#064e3b",
    hotHex: "#ecfdf5",
    cssAccent: "oklch(76% 0.15 162)",
    cssAccentHover: "oklch(81% 0.14 162)",
    cssAccentSoft: "oklch(23.5% 0.05 162 / 0.75)",
    cssCool: "oklch(77% 0.13 192)",
    cssCoolSoft: "oklch(23.5% 0.045 192 / 0.75)",
  },
  {
    id: "crimson",
    label: "Event Quasar",
    primaryHex: "#fb7185",
    secondaryHex: "#7f1d1d",
    hotHex: "#fff1f2",
    cssAccent: "oklch(71% 0.18 22)",
    cssAccentHover: "oklch(76% 0.17 28)",
    cssAccentSoft: "oklch(24% 0.055 22 / 0.75)",
    cssCool: "oklch(76% 0.15 48)",
    cssCoolSoft: "oklch(24% 0.05 48 / 0.75)",
  },
];

const VERTEX_SHADER = `
  varying vec2 vUv;
  void main() {
    vUv = uv;
    gl_Position = vec4(position, 1.0);
  }
`;

// Exact Binet General Relativistic Geodesic Raytracer:
// Solves d2u/dphi2 + u = 1.5 * Rs * u^2 (u = 1/r) in the photon's exact 2D orbital plane
// with analytical equatorial crossing angles phi_k (0% stepping artifacts, full 2*PI lensed arches).
const FRAGMENT_SHADER = `
  precision highp float;

  varying vec2 vUv;
  uniform float uTime;
  uniform vec2 uResolution;
  uniform vec2 uPointer;
  uniform float uScroll;
  uniform float uWheelBoost;
  uniform vec3 uColorPrimary;
  uniform vec3 uColorSecondary;
  uniform vec3 uColorHot;

  #define PI 3.14159265359

  mat2 rot2(float a) {
    float c = cos(a), s = sin(a);
    return mat2(c, -s, s, c);
  }

  float hash21(vec2 p) {
    vec3 p3 = fract(vec3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
  }

  float noise2(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    f = f * f * (3.0 - 2.0 * f);
    float a = hash21(i);
    float b = hash21(i + vec2(1.0, 0.0));
    float c = hash21(i + vec2(0.0, 1.0));
    float d = hash21(i + vec2(1.0, 1.0));
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
  }

  float fbm2(vec2 p) {
    float v = 0.0;
    float a = 0.5;
    mat2 m = mat2(0.80, 0.60, -0.60, 0.80);
    for (int i = 0; i < 6; i++) {
      v += a * noise2(p);
      p = m * p * 2.04;
      a *= 0.5;
    }
    return v;
  }

  vec3 acesFilm(vec3 x) {
    const float a = 2.51;
    const float b = 0.03;
    const float c = 2.43;
    const float d = 0.59;
    const float e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), 0.0, 1.0);
  }

  // Evaluate relativistic accretion disk at exact equatorial crossing point xz = (x, z)
  vec4 evaluateAccretionDisk(vec2 xz, vec3 photonDir, float time, float wheelBoost) {
    float r = length(xz);
    float rInner = 1.08;
    float rOuter = 9.5;

    if (r <= rInner || r >= rOuter) {
      return vec4(0.0);
    }

    // Ultra-smooth radial envelope: soft inner horizon taper + long exponential outer nebula fade
    float innerFade = smoothstep(rInner, rInner + 1.25, r);
    float outerFade = exp(-max(r - 2.0, 0.0) * 0.54) * smoothstep(rOuter, rOuter - 3.2, r);
    float radialEnv = innerFade * outerFade;

    // 100% seamless 2*PI-periodic Cartesian spiral coordinates
    float shearTwist = 3.8 * log(max(r, 1.0)) - time * (0.78 + wheelBoost * 0.85);
    vec2 sp1 = rot2(shearTwist) * (xz * 0.72);
    vec2 sp2 = rot2(shearTwist * 1.45 - time * 0.30) * (xz * 1.35);

    float turb1 = fbm2(sp1);
    float turb2 = fbm2(sp2 + vec2(turb1 * 1.6));

    // Smooth large-scale orbital plasma streams (no high-frequency aliasing)
    vec2 unitDir = xz / r;
    float stream = fbm2(unitDir * 2.1 + vec2(log(r) * 3.2 - time * 0.22, turb1 * 1.4));

    float filaments = turb1 * 0.44 + turb2 * 0.32 + stream * 0.24;
    filaments = smoothstep(0.18, 0.78, filaments);

    // Soft volumetric dust lanes
    float dustLane = smoothstep(0.24, 0.70, (turb1 * 0.6 + stream * 0.55));
    float density = (0.28 + 0.72 * filaments) * mix(0.52, 1.0, dustLane) * radialEnv;

    // Relativistic Doppler Beaming + Gravitational Redshift
    vec3 diskTangent = normalize(vec3(-xz.y, 0.0, xz.x));
    float beta = clamp(0.54 / sqrt(max(r - 0.65, 0.45)), 0.0, 0.64);
    float cosTheta = dot(diskTangent, -photonDir);
    float gravRedshift = sqrt(clamp(1.0 - 0.92 / max(r, 1.02), 0.24, 1.0));
    float doppler = clamp(gravRedshift / (1.0 - beta * cosTheta), 0.36, 1.92);
    float beaming = pow(doppler, 2.40);

    // Physical blackbody temperature gradient across the disk
    float temp = clamp(
      pow(1.50 / max(r, 1.35), 0.76) * doppler * (0.72 + 0.46 * filaments),
      0.0,
      1.6
    );

    vec3 outerDust = uColorSecondary * 0.50;
    vec3 mainPlasma = uColorPrimary * 1.24;
    vec3 hotCore = mix(uColorPrimary, uColorHot, 0.92) * 1.92;

    vec3 col = mix(outerDust, mainPlasma, smoothstep(0.05, 0.48, temp));
    col = mix(col, hotCore, smoothstep(0.42, 1.15, temp));

    float alpha = clamp(density * 0.52, 0.0, 0.68);
    vec3 emission = col * density * beaming * (2.25 + wheelBoost * 0.85);

    return vec4(emission, alpha);
  }

  vec3 sampleStarfield(vec3 dir) {
    vec3 p = dir * 210.0;
    vec3 id = floor(p);
    vec3 f = fract(p) - 0.5;
    float h = hash21(id.xy + id.z * 41.0);
    vec3 col = vec3(0.003, 0.004, 0.007);
    if (h > 0.982) {
      float d = length(f);
      float spark = smoothstep(0.16, 0.0, d);
      spark *= 0.75 + 0.25 * sin(uTime * 2.1 + h * 95.0);
      col += mix(uColorHot, uColorPrimary, fract(h * 23.7) * 0.4) * spark * 0.85;
    }
    return col;
  }

  void main() {
    vec2 uv = (vUv - 0.5) * 2.0;
    float aspect = uResolution.x / max(uResolution.y, 1.0);
    uv.x *= aspect;

    // Frame Gargantua slightly right of center on desktop so hero text on the left is unobstructed
    float shiftX = aspect > 1.1 ? 0.34 * (1.0 - 0.35 * uScroll) : 0.0;
    vec2 p = uv - vec2(shiftX, 0.02 - (uScroll - 0.3) * 0.08);

    // Camera positioned at r0 = 11.2 Rs with shallow ~7.8 deg inclination
    float r0 = 11.2 - sin(uScroll * PI) * 1.0;
    float elev = clamp(0.136 + uPointer.y * 0.06 + sin(uScroll * PI * 0.8) * 0.075, 0.055, 0.35);
    float azim = -0.30 + uPointer.x * 0.14 + uScroll * 0.32;

    vec3 ro = vec3(
      r0 * cos(elev) * sin(azim),
      r0 * sin(elev),
      r0 * cos(elev) * cos(azim)
    );

    vec3 ww = normalize(-ro);
    float roll = -0.21 + uPointer.x * 0.04;
    vec3 up = normalize(vec3(sin(roll), cos(roll), 0.0));
    vec3 uu = normalize(cross(ww, up));
    vec3 vv = normalize(cross(uu, ww));

    vec3 rd = normalize(p.x * uu + p.y * vv + 1.85 * ww);

    // Construct the photon's exact 2D orbital plane basis (e1, e2)
    vec3 e1 = ro / r0;
    float cosAlpha = dot(rd, e1);
    vec3 transVec = rd - cosAlpha * e1;
    float sinAlpha = length(transVec);
    vec3 e2 = transVec / max(sinAlpha, 0.00001);

    // Binet initial conditions at phi = 0: u(0) = 1/r0, du/dphi(0) = -cot(alpha) / r0
    float u = 1.0 / r0;
    float du = -cosAlpha / max(r0 * sinAlpha, 0.00001);
    float maxU = u;

    // 1) Primary disk + upper lensed arch: ONE single C-infinity smooth angle across the entire screen!
    // Since e1.y > 0, atan(e1.y, -e2.y) smoothly spans (0, PI) without any branch at e2.y == 0.
    float primaryCrossPhi = atan(e1.y, -e2.y);

    // 2) Lower lensed arch (underneath the black hole when e2.y < 0):
    float lowerArchWeight = smoothstep(-0.025, -0.19, e2.y);
    float lowerArchPhi = PI - atan(e1.y * 0.32, max(-e2.y, 0.0001));

    vec3 accumColor = vec3(0.0);
    float transmittance = 1.0;
    bool captured = false;

    float phi = 0.0;
    float dphi = 0.044;

    for (int i = 0; i < 120; i++) {
      // RK4 integration of Binet's exact GR equation: d2u/dphi2 = 1.5 * u^2 - u
      float k1_u = du;
      float k1_du = 1.5 * u * u - u;

      float u2 = u + 0.5 * dphi * k1_u;
      float du2 = du + 0.5 * dphi * k1_du;
      float k2_u = du2;
      float k2_du = 1.5 * u2 * u2 - u2;

      float u3 = u + 0.5 * dphi * k2_u;
      float du3 = du + 0.5 * dphi * k2_du;
      float k3_u = du3;
      float k3_du = 1.5 * u3 * u3 - u3;

      float u4 = u + dphi * k3_u;
      float du4 = du + dphi * k3_du;
      float k4_u = du4;
      float k4_du = 1.5 * u4 * u4 - u4;

      float nextU = u + (dphi / 6.0) * (k1_u + 2.0 * k2_u + 2.0 * k3_u + k4_u);
      float nextDu = du + (dphi / 6.0) * (k1_du + 2.0 * k2_du + 2.0 * k3_du + k4_du);
      float nextPhi = phi + dphi;

      // 1. Primary Equatorial Disk + Upper Lensed Arch (100% branchless & seamless across the wings)
      if (phi < primaryCrossPhi && nextPhi >= primaryCrossPhi) {
        float dtPhi = primaryCrossPhi - phi;
        float d2u = 1.5 * u * u - u;
        float uCross = u + du * dtPhi + 0.5 * d2u * dtPhi * dtPhi;
        float duCross = du + d2u * dtPhi;

        if (uCross > 0.02 && uCross < 0.98) {
          float rCross = 1.0 / uCross;
          vec3 rHat = cos(primaryCrossPhi) * e1 + sin(primaryCrossPhi) * e2;
          vec3 thetaHat = -sin(primaryCrossPhi) * e1 + cos(primaryCrossPhi) * e2;
          vec3 hitPos = rCross * rHat;
          vec3 photonDir = normalize(uCross * thetaHat - duCross * rHat);

          vec4 diskSample = evaluateAccretionDisk(hitPos.xz, photonDir, uTime, uWheelBoost);
          if (diskSample.a > 0.001) {
            accumColor += transmittance * diskSample.rgb;
            transmittance *= (1.0 - diskSample.a);
          }
        }
      }

      // 2. Lower Gravitationally Lensed Einstein Arch (underneath the black hole shadow)
      if (lowerArchWeight > 0.001 && phi < lowerArchPhi && nextPhi >= lowerArchPhi) {
        float dtPhi = lowerArchPhi - phi;
        float d2u = 1.5 * u * u - u;
        float uCross = u + du * dtPhi + 0.5 * d2u * dtPhi * dtPhi;
        float duCross = du + d2u * dtPhi;

        if (uCross > 0.02 && uCross < 0.98) {
          float rCross = 1.0 / uCross;
          vec3 rHat = cos(lowerArchPhi) * e1 + sin(lowerArchPhi) * e2;
          vec3 thetaHat = -sin(lowerArchPhi) * e1 + cos(lowerArchPhi) * e2;
          vec3 hitPos = rCross * rHat;
          vec3 photonDir = normalize(uCross * thetaHat - duCross * rHat);

          vec4 diskSample = evaluateAccretionDisk(hitPos.xz, photonDir, uTime, uWheelBoost);
          if (diskSample.a > 0.001) {
            accumColor += transmittance * diskSample.rgb * lowerArchWeight;
            transmittance *= (1.0 - diskSample.a * lowerArchWeight);
          }
        }
      }

      maxU = max(maxU, nextU);

      // Event Horizon capture (r <= 1.0 Rs  <=>  u >= 1.0)
      if (nextU >= 1.0) {
        captured = true;
        break;
      }

      // Escaped to asymptotic infinity (u <= 0)
      if (nextU <= 0.0 && nextDu < 0.0) {
        float frac = u / max(u - nextU, 0.00001);
        phi = mix(phi, nextPhi, frac);
        u = 0.0;
        du = mix(du, nextDu, frac);
        break;
      }

      if (transmittance < 0.01) {
        break;
      }

      u = nextU;
      du = nextDu;
      phi = nextPhi;
    }

    // Relativistic Photon Ring right at the Photon Sphere boundary (r_min = 1.5 Rs <=> maxU = 0.6667)
    float photonSphereDist = abs(maxU - 0.6667);
    float photonRing = exp(-photonSphereDist * 34.0) * 1.45 + exp(-photonSphereDist * 8.5) * 0.20;
    float ringAsym = 0.80 + 0.35 * clamp(-p.x / max(length(p), 0.15), -1.0, 1.0);
    vec3 ringGlow = mix(uColorPrimary, uColorHot, 0.82) * photonRing * ringAsym * (1.0 + uWheelBoost * 0.4);

    if (!captured) {
      vec3 escapedDir = cos(phi) * e1 + sin(phi) * e2;
      accumColor += transmittance * (sampleStarfield(escapedDir) + ringGlow);
      // Soft atmospheric corona only outside the event horizon shadow
      float dCenter = length(p);
      float corona = exp(-dCenter * 1.35) * 0.09;
      accumColor += transmittance * mix(uColorSecondary, uColorPrimary, 0.55) * corona;
    } else {
      // Razor-sharp antialiased photon rim right on the edge of the pure black Event Horizon shadow
      float shadowEdge = smoothstep(1.18, 0.67, maxU);
      accumColor += transmittance * ringGlow * shadowEdge * 0.28;
    }

    vec3 col = acesFilm(accumColor * 1.15);
    col = pow(col, vec3(0.95));

    // Readability vignette on the left so Hero headline & buttons have high contrast
    float leftReadability = smoothstep(-aspect * 1.05, aspect * 0.12, uv.x);
    col *= mix(0.45, 1.0, leftReadability);

    gl_FragColor = vec4(col, 1.0);
  }
`;

export function BlackHoleBackground() {
  const mountRef = useRef<HTMLDivElement | null>(null);
  const [autoMode, setAutoMode] = useState<boolean>(true);
  const [activeThemeIndex, setActiveThemeIndex] = useState<number>(0);

  const autoModeRef = useRef<boolean>(true);
  const targetThemeIndexRef = useRef<number>(0);

  useEffect(() => {
    autoModeRef.current = autoMode;
  }, [autoMode]);

  useEffect(() => {
    targetThemeIndexRef.current = activeThemeIndex;
  }, [activeThemeIndex]);

  useEffect(() => {
    const container = mountRef.current;
    if (!container) return;

    const width = window.innerWidth || 1280;
    const height = window.innerHeight || 800;

    const scene = new THREE.Scene();
    const orthoCamera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0, 10);

    let renderer: THREE.WebGLRenderer;
    try {
      renderer = new THREE.WebGLRenderer({
        antialias: true,
        powerPreference: "high-performance",
      });
    } catch {
      return;
    }

    renderer.setSize(width, height);
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 1.75));
    container.innerHTML = "";
    container.appendChild(renderer.domElement);

    const initialTheme = VARYNTH_THEMES[0];
    const uniforms = {
      uTime: { value: 0 },
      uResolution: { value: new THREE.Vector2(width, height) },
      uPointer: { value: new THREE.Vector2(0, 0) },
      uScroll: { value: 0 },
      uWheelBoost: { value: 0 },
      uColorPrimary: { value: new THREE.Color(initialTheme.primaryHex) },
      uColorSecondary: { value: new THREE.Color(initialTheme.secondaryHex) },
      uColorHot: { value: new THREE.Color(initialTheme.hotHex) },
    };

    const quadGeo = new THREE.PlaneGeometry(2, 2);
    const quadMat = new THREE.ShaderMaterial({
      vertexShader: VERTEX_SHADER,
      fragmentShader: FRAGMENT_SHADER,
      uniforms,
      depthWrite: false,
      depthTest: false,
    });
    const quadMesh = new THREE.Mesh(quadGeo, quadMat);
    scene.add(quadMesh);

    let pointerX = 0;
    let pointerY = 0;
    let smoothPointerX = 0;
    let smoothPointerY = 0;
    let targetScroll = 0;
    let smoothScroll = 0;
    let wheelBoost = 0;

    const onScroll = () => {
      const maxScroll = Math.max(
        document.documentElement.scrollHeight - window.innerHeight,
        1
      );
      targetScroll = Math.min(Math.max(window.scrollY / maxScroll, 0), 1);
    };

    const onWheel = (e: WheelEvent) => {
      const impulse = Math.min(Math.abs(e.deltaY) * 0.0022, 0.65);
      wheelBoost = Math.min(wheelBoost + impulse, 1.6);
    };

    const onMouseMove = (e: MouseEvent) => {
      pointerX = (e.clientX / window.innerWidth - 0.5) * 2;
      pointerY = (e.clientY / window.innerHeight - 0.5) * 2;
    };

    const onResize = () => {
      const w = window.innerWidth || 1280;
      const h = window.innerHeight || 800;
      renderer.setSize(w, h);
      uniforms.uResolution.value.set(w, h);
    };

    window.addEventListener("scroll", onScroll, { passive: true });
    window.addEventListener("wheel", onWheel, { passive: true });
    window.addEventListener("mousemove", onMouseMove, { passive: true });
    window.addEventListener("resize", onResize);
    onScroll();

    const clock = new THREE.Clock();
    let frameId = 0;
    let lastCssSyncTime = -1;
    let lastDisplayedIndex = 0;

    const colorAPrimary = new THREE.Color();
    const colorBPrimary = new THREE.Color();
    const colorASecondary = new THREE.Color();
    const colorBSecondary = new THREE.Color();
    const colorAHot = new THREE.Color();
    const colorBHot = new THREE.Color();

    const applyThemeCssTokens = (theme: VarynthThemeSpec) => {
      const root = document.documentElement;
      root.style.setProperty("--color-accent", theme.cssAccent);
      root.style.setProperty("--color-accent-hover", theme.cssAccentHover);
      root.style.setProperty("--color-accent-soft", theme.cssAccentSoft);
      root.style.setProperty("--color-focus", theme.cssAccent);
      root.style.setProperty("--color-cool", theme.cssCool);
      root.style.setProperty("--color-cool-soft", theme.cssCoolSoft);
    };

    applyThemeCssTokens(VARYNTH_THEMES[0]);

    const animate = () => {
      frameId = requestAnimationFrame(animate);
      const elapsed = clock.getElapsedTime();

      smoothScroll += (targetScroll - smoothScroll) * 0.07;
      smoothPointerX += (pointerX - smoothPointerX) * 0.05;
      smoothPointerY += (pointerY - smoothPointerY) * 0.05;
      wheelBoost *= 0.93;

      uniforms.uTime.value = elapsed;
      uniforms.uScroll.value = smoothScroll;
      uniforms.uPointer.value.set(smoothPointerX, smoothPointerY);
      uniforms.uWheelBoost.value = wheelBoost;

      if (autoModeRef.current) {
        const cycleDuration = 11.0;
        const totalPos = elapsed / cycleDuration;
        const idxA = Math.floor(totalPos) % VARYNTH_THEMES.length;
        const idxB = (idxA + 1) % VARYNTH_THEMES.length;
        const t = totalPos - Math.floor(totalPos);
        const smoothT = t * t * (3 - 2 * t);

        const themeA = VARYNTH_THEMES[idxA];
        const themeB = VARYNTH_THEMES[idxB];

        colorAPrimary.set(themeA.primaryHex);
        colorBPrimary.set(themeB.primaryHex);
        uniforms.uColorPrimary.value.copy(colorAPrimary).lerp(colorBPrimary, smoothT);

        colorASecondary.set(themeA.secondaryHex);
        colorBSecondary.set(themeB.secondaryHex);
        uniforms.uColorSecondary.value.copy(colorASecondary).lerp(colorBSecondary, smoothT);

        colorAHot.set(themeA.hotHex);
        colorBHot.set(themeB.hotHex);
        uniforms.uColorHot.value.copy(colorAHot).lerp(colorBHot, smoothT);

        const dominantIdx = smoothT > 0.5 ? idxB : idxA;
        if (dominantIdx !== lastDisplayedIndex || elapsed - lastCssSyncTime > 5.5) {
          lastDisplayedIndex = dominantIdx;
          lastCssSyncTime = elapsed;
          setActiveThemeIndex(dominantIdx);
          applyThemeCssTokens(VARYNTH_THEMES[dominantIdx]);
        }
      } else {
        const targetTheme = VARYNTH_THEMES[targetThemeIndexRef.current] || VARYNTH_THEMES[0];
        colorBPrimary.set(targetTheme.primaryHex);
        colorBSecondary.set(targetTheme.secondaryHex);
        colorBHot.set(targetTheme.hotHex);

        uniforms.uColorPrimary.value.lerp(colorBPrimary, 0.08);
        uniforms.uColorSecondary.value.lerp(colorBSecondary, 0.08);
        uniforms.uColorHot.value.lerp(colorBHot, 0.08);
      }

      // Continuously sync --color-accent with the exact live Black Hole primary color
      const liveHex = `#${uniforms.uColorPrimary.value.getHexString()}`;
      document.documentElement.style.setProperty("--color-accent", liveHex);
      document.documentElement.style.setProperty("--color-accent-hover", liveHex);
      document.documentElement.style.setProperty("--color-focus", liveHex);

      renderer.render(scene, orthoCamera);
    };

    animate();

    return () => {
      cancelAnimationFrame(frameId);
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("wheel", onWheel);
      window.removeEventListener("mousemove", onMouseMove);
      window.removeEventListener("resize", onResize);
      renderer.dispose();
    };
  }, []);

  const handleSelectTheme = (idx: number) => {
    setAutoMode(false);
    setActiveThemeIndex(idx);
    const chosen = VARYNTH_THEMES[idx];
    if (chosen && typeof document !== "undefined") {
      const root = document.documentElement;
      root.style.setProperty("--color-accent", chosen.cssAccent);
      root.style.setProperty("--color-accent-hover", chosen.cssAccentHover);
      root.style.setProperty("--color-accent-soft", chosen.cssAccentSoft);
      root.style.setProperty("--color-focus", chosen.cssAccent);
      root.style.setProperty("--color-cool", chosen.cssCool);
      root.style.setProperty("--color-cool-soft", chosen.cssCoolSoft);
    }
  };

  const currentTheme = VARYNTH_THEMES[activeThemeIndex] || VARYNTH_THEMES[0];

  return (
    <>
      <div
        ref={mountRef}
        className="blackhole-canvas-backdrop"
        aria-hidden="true"
      />
      <aside
        className="blackhole-theme-dock"
        aria-label="Varynth singularity theme controller"
      >
        <button
          type="button"
          className={autoMode ? "bh-mode-btn active" : "bh-mode-btn"}
          onClick={() => setAutoMode((prev) => !prev)}
          title="Automatically morph singularity colors across Varynth themes"
        >
          <span className="bh-pulse-dot" />
          <span>{autoMode ? `Auto · ${currentTheme.label}` : currentTheme.label}</span>
        </button>
        <div className="bh-swatch-group" role="group" aria-label="Select Varynth theme">
          {VARYNTH_THEMES.map((t, idx) => (
            <button
              key={t.id}
              type="button"
              className={
                !autoMode && activeThemeIndex === idx
                  ? "bh-swatch-btn active"
                  : "bh-swatch-btn"
              }
              onClick={() => handleSelectTheme(idx)}
              aria-label={t.label}
              title={t.label}
              data-theme-id={t.id}
            />
          ))}
        </div>
      </aside>
    </>
  );
}
