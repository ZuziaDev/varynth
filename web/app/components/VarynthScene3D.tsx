"use client";

import React, { useEffect, useRef, useState } from "react";
import * as THREE from "three";

export function VarynthScene3D() {
  const mountRef = useRef<HTMLDivElement | null>(null);
  const [scrollPct, setScrollPct] = useState(0);
  const [wheelTorque, setWheelTorque] = useState(0);
  const [activeStage, setActiveStage] = useState("STAGE 01 · 61 LIVE MODELS");

  useEffect(() => {
    const container = mountRef.current;
    if (!container) return;

    const width = container.clientWidth || 560;
    const height = container.clientHeight || 520;

    const scene = new THREE.Scene();
    const camera = new THREE.PerspectiveCamera(42, width / height, 0.1, 100);
    camera.position.set(0, 0.2, 6.4);

    let renderer: THREE.WebGLRenderer;
    try {
      renderer = new THREE.WebGLRenderer({
        alpha: true,
        antialias: true,
        powerPreference: "high-performance",
      });
    } catch {
      return;
    }

    renderer.setSize(width, height);
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
    renderer.toneMapping = THREE.ACESFilmicToneMapping;
    renderer.toneMappingExposure = 1.2;
    container.innerHTML = "";
    container.appendChild(renderer.domElement);

    const rootGroup = new THREE.Group();
    scene.add(rootGroup);

    // 1. Inner Faceted Event-Horizon Core
    const coreGeo = new THREE.IcosahedronGeometry(1.16, 1);
    const coreMat = new THREE.MeshPhysicalMaterial({
      color: new THREE.Color("#0b0c12"),
      emissive: new THREE.Color("#1f1135"),
      emissiveIntensity: 0.65,
      metalness: 0.82,
      roughness: 0.18,
      clearcoat: 0.95,
      clearcoatRoughness: 0.12,
      flatShading: true,
    });
    const coreMesh = new THREE.Mesh(coreGeo, coreMat);
    rootGroup.add(coreMesh);

    // 2. Glowing Wireframe Exoskeleton around the Core
    const exoGeo = new THREE.IcosahedronGeometry(1.23, 1);
    const exoEdges = new THREE.EdgesGeometry(exoGeo);
    const exoMat = new THREE.LineBasicMaterial({
      color: new THREE.Color("#a855f7"),
      transparent: true,
      opacity: 0.78,
    });
    const exoLines = new THREE.LineSegments(exoEdges, exoMat);
    rootGroup.add(exoLines);

    // 3. Inner Octahedral Singularity Seed
    const seedGeo = new THREE.OctahedronGeometry(0.62, 0);
    const seedMat = new THREE.MeshBasicMaterial({
      color: new THREE.Color("#f5d0fe"),
      wireframe: true,
      transparent: true,
      opacity: 0.72,
    });
    const seedMesh = new THREE.Mesh(seedGeo, seedMat);
    rootGroup.add(seedMesh);

    // 4. Three Gyroscopic Photon Rings
    const ringsGroup = new THREE.Group();
    rootGroup.add(ringsGroup);

    const ringConfigs = [
      { radius: 1.78, tube: 0.013, color: "#a855f7", opacity: 0.75, rx: 1.1, ry: 0.3 },
      { radius: 2.12, tube: 0.011, color: "#6366f1", opacity: 0.55, rx: -0.7, ry: 0.9 },
      { radius: 2.46, tube: 0.009, color: "#f5d0fe", opacity: 0.35, rx: 0.4, ry: -1.2 },
    ];

    const ringMeshes: THREE.Mesh[] = [];
    const ringMaterials: THREE.MeshBasicMaterial[] = [];
    ringConfigs.forEach((cfg) => {
      const rGeo = new THREE.TorusGeometry(cfg.radius, cfg.tube, 16, 120);
      const rMat = new THREE.MeshBasicMaterial({
        color: new THREE.Color(cfg.color),
        transparent: true,
        opacity: cfg.opacity,
      });
      const rMesh = new THREE.Mesh(rGeo, rMat);
      rMesh.rotation.set(cfg.rx, cfg.ry, 0);
      ringsGroup.add(rMesh);
      ringMeshes.push(rMesh);
      ringMaterials.push(rMat);
    });

    // 5. 61 Model Constellation Nodes + Synaptic Connections
    const NODE_COUNT = 61;
    const nodePositions = new Float32Array(NODE_COUNT * 3);
    const baseNodeVectors: THREE.Vector3[] = [];

    for (let i = 0; i < NODE_COUNT; i++) {
      const phi = Math.acos(1 - (2 * (i + 0.5)) / NODE_COUNT);
      const theta = Math.PI * (1 + Math.sqrt(5)) * i;
      const rad = 1.95 + (i % 5) * 0.18;
      const x = rad * Math.sin(phi) * Math.cos(theta);
      const y = rad * Math.sin(phi) * Math.sin(theta);
      const z = rad * Math.cos(phi);
      nodePositions[i * 3] = x;
      nodePositions[i * 3 + 1] = y;
      nodePositions[i * 3 + 2] = z;
      baseNodeVectors.push(new THREE.Vector3(x, y, z));
    }

    const pointsGeo = new THREE.BufferGeometry();
    pointsGeo.setAttribute("position", new THREE.BufferAttribute(nodePositions, 3));
    const pointsMat = new THREE.PointsMaterial({
      color: new THREE.Color("#f5d0fe"),
      size: 0.058,
      transparent: true,
      opacity: 0.92,
    });
    const constellationPoints = new THREE.Points(pointsGeo, pointsMat);
    rootGroup.add(constellationPoints);

    const lineCoords: number[] = [];
    for (let i = 0; i < NODE_COUNT; i++) {
      for (let j = i + 1; j < NODE_COUNT; j++) {
        if (baseNodeVectors[i].distanceTo(baseNodeVectors[j]) < 0.92) {
          lineCoords.push(
            baseNodeVectors[i].x,
            baseNodeVectors[i].y,
            baseNodeVectors[i].z,
            baseNodeVectors[j].x,
            baseNodeVectors[j].y,
            baseNodeVectors[j].z
          );
        }
      }
    }
    const synGeo = new THREE.BufferGeometry();
    synGeo.setAttribute("position", new THREE.Float32BufferAttribute(lineCoords, 3));
    const synMat = new THREE.LineBasicMaterial({
      color: new THREE.Color("#6366f1"),
      transparent: true,
      opacity: 0.22,
    });
    const synLines = new THREE.LineSegments(synGeo, synMat);
    rootGroup.add(synLines);

    // 6. Dynamic Theme-Reactive Lighting
    const ambient = new THREE.AmbientLight("#ffffff", 0.65);
    scene.add(ambient);

    const primaryLight = new THREE.PointLight("#a855f7", 7.2, 14);
    primaryLight.position.set(3.2, 2.4, 3.5);
    scene.add(primaryLight);

    const secondaryLight = new THREE.PointLight("#6366f1", 5.6, 14);
    secondaryLight.position.set(-3.4, -2.2, 2.8);
    scene.add(secondaryLight);

    const rimLight = new THREE.DirectionalLight("#f5f3ef", 2.2);
    rimLight.position.set(0, 4, -3);
    scene.add(rimLight);

    // Target colors synced with BlackHoleBackground theme engine
    const targetPrimary = new THREE.Color("#a855f7");
    const targetSecondary = new THREE.Color("#6366f1");
    const targetHot = new THREE.Color("#f5d0fe");

    const onThemeChange = (e: Event) => {
      const custom = e as CustomEvent<{
        primaryHex: string;
        secondaryHex: string;
        hotHex: string;
      }>;
      if (custom.detail) {
        targetPrimary.set(custom.detail.primaryHex);
        targetSecondary.set(custom.detail.secondaryHex);
        targetHot.set(custom.detail.hotHex);
      }
    };

    window.addEventListener("varynth-theme-change", onThemeChange);

    let targetScroll = 0;
    let smoothScroll = 0;
    let wheelVelocity = 0;
    let pointerX = 0;
    let pointerY = 0;
    let smoothPointerX = 0;
    let smoothPointerY = 0;

    const updateScroll = () => {
      const maxScroll = Math.max(
        document.documentElement.scrollHeight - window.innerHeight,
        1
      );
      targetScroll = Math.min(Math.max(window.scrollY / maxScroll, 0), 1);
      const pct = Math.round(targetScroll * 100);
      setScrollPct(pct);

      if (targetScroll < 0.2) {
        setActiveStage("STAGE 01 · 61 LIVE MODELS");
      } else if (targetScroll < 0.42) {
        setActiveStage("STAGE 02 · MULTI-OS INSTALL");
      } else if (targetScroll < 0.65) {
        setActiveStage("STAGE 03 · 5 SUBSYSTEMS");
      } else if (targetScroll < 0.85) {
        setActiveStage("STAGE 04 · 7-ROLE ROUTING");
      } else {
        setActiveStage("STAGE 05 · LOCAL SOVEREIGNTY");
      }
    };

    const onWheel = (e: WheelEvent) => {
      const boost = Math.max(Math.min(e.deltaY * 0.0018, 0.35), -0.35);
      wheelVelocity += boost;
      setWheelTorque(Math.min(100, Math.round(Math.abs(wheelVelocity) * 280)));
    };

    const onPointerMove = (e: MouseEvent) => {
      pointerX = (e.clientX / window.innerWidth - 0.5) * 2;
      pointerY = (e.clientY / window.innerHeight - 0.5) * 2;
    };

    const onResize = () => {
      if (!container) return;
      const w = container.clientWidth || 560;
      const h = container.clientHeight || 520;
      camera.aspect = w / h;
      camera.updateProjectionMatrix();
      renderer.setSize(w, h);
    };

    window.addEventListener("scroll", updateScroll, { passive: true });
    window.addEventListener("wheel", onWheel, { passive: true });
    window.addEventListener("mousemove", onPointerMove, { passive: true });
    window.addEventListener("resize", onResize);
    updateScroll();

    let frameId = 0;
    const clock = new THREE.Clock();

    const animate = () => {
      frameId = requestAnimationFrame(animate);
      const elapsed = clock.getElapsedTime();

      smoothScroll += (targetScroll - smoothScroll) * 0.08;
      wheelVelocity *= 0.92;
      smoothPointerX += (pointerX - smoothPointerX) * 0.06;
      smoothPointerY += (pointerY - smoothPointerY) * 0.06;

      // Smoothly morph 3D scene colors to match the Black Hole theme
      exoMat.color.lerp(targetPrimary, 0.06);
      seedMat.color.lerp(targetHot, 0.06);
      pointsMat.color.lerp(targetHot, 0.06);
      synMat.color.lerp(targetSecondary, 0.06);
      primaryLight.color.lerp(targetPrimary, 0.06);
      secondaryLight.color.lerp(targetSecondary, 0.06);
      coreMat.emissive.lerp(targetPrimary, 0.04);
      if (ringMaterials[0]) ringMaterials[0].color.lerp(targetPrimary, 0.06);
      if (ringMaterials[1]) ringMaterials[1].color.lerp(targetSecondary, 0.06);
      if (ringMaterials[2]) ringMaterials[2].color.lerp(targetHot, 0.06);

      const baseSpin = elapsed * 0.28 + smoothScroll * Math.PI * 2.4;
      coreMesh.rotation.y = baseSpin + wheelVelocity * 2.2;
      coreMesh.rotation.x = elapsed * 0.18 + smoothScroll * 1.4;

      exoLines.rotation.y = coreMesh.rotation.y;
      exoLines.rotation.x = coreMesh.rotation.x;

      seedMesh.rotation.y = -elapsed * 0.55 - smoothScroll * Math.PI * 3;
      seedMesh.rotation.z = elapsed * 0.35 + wheelVelocity * 3;

      const pulse = 1 + Math.sin(elapsed * 2.2) * 0.035 + Math.abs(wheelVelocity) * 0.45;
      exoLines.scale.setScalar(pulse);
      constellationPoints.scale.setScalar(1 + smoothScroll * 0.18 + Math.abs(wheelVelocity) * 0.35);
      synLines.scale.copy(constellationPoints.scale);

      if (ringMeshes[0]) {
        ringMeshes[0].rotation.z = elapsed * 0.42 + smoothScroll * 4.2 + wheelVelocity * 2.5;
        ringMeshes[0].rotation.x = 1.1 + smoothScroll * 0.8;
      }
      if (ringMeshes[1]) {
        ringMeshes[1].rotation.y = -elapsed * 0.34 - smoothScroll * 3.6 - wheelVelocity * 2.0;
        ringMeshes[1].rotation.x = -0.7 + Math.sin(elapsed * 0.6) * 0.25;
      }
      if (ringMeshes[2]) {
        ringMeshes[2].rotation.z = -elapsed * 0.25 + smoothScroll * 2.8;
        ringMeshes[2].rotation.y = -1.2 + smoothScroll * 1.1;
      }

      rootGroup.rotation.y = smoothPointerX * 0.42 + smoothScroll * 0.65;
      rootGroup.rotation.x = smoothPointerY * 0.28 + Math.sin(smoothScroll * Math.PI) * 0.22;
      camera.position.z = 6.4 - Math.sin(smoothScroll * Math.PI) * 0.65;

      renderer.render(scene, camera);
    };

    animate();

    return () => {
      cancelAnimationFrame(frameId);
      window.removeEventListener("varynth-theme-change", onThemeChange);
      window.removeEventListener("scroll", updateScroll);
      window.removeEventListener("wheel", onWheel);
      window.removeEventListener("mousemove", onPointerMove);
      window.removeEventListener("resize", onResize);
      renderer.dispose();
    };
  }, []);

  return (
    <div
      className="webgl-stage-card"
      data-slot="hero-3d-core"
      data-kind="product"
      aria-label="Interactive Three.js 3D Neural Lattice"
    >
      <div className="webgl-hud-top">
        <span className="webgl-hud-pill">
          <span className="hud-dot" />
          {activeStage}
        </span>
        <span className="webgl-hud-meta">SINGULARITY LOCK · 60FPS</span>
      </div>

      <div ref={mountRef} className="webgl-canvas-mount" />

      <div className="webgl-hud-bottom">
        <div className="hud-metric">
          <span className="hud-metric-label">Scroll orbit</span>
          <span className="hud-metric-val">{scrollPct}%</span>
        </div>
        <div className="hud-metric">
          <span className="hud-metric-label">Wheel torque</span>
          <span className="hud-metric-val">{wheelTorque} rad/s</span>
        </div>
        <div className="hud-metric">
          <span className="hud-metric-label">Active nodes</span>
          <span className="hud-metric-val">61 models · 7 roles</span>
        </div>
      </div>
    </div>
  );
}
