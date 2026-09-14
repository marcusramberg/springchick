// Pull the chick down, let go, it springs back. Same idea as the compositor's
// interruptible springs: grabbing mid-flight takes over the current offset.
(() => {
  const host = document.querySelector(".chick");
  if (!host || matchMedia("(prefers-reduced-motion: reduce)").matches) return;

  const el = (id) => host.querySelector("#" + id);
  const chick = el("sc-chick"), legs = el("sc-legs"), body = el("sc-body");
  const wingL = el("sc-wing-l"), wingR = el("sc-wing-r");
  if (!chick || !legs || !body || !wingL || !wingR) return;

  const LEG_LEN = 59; // px in viewBox units, spring top 411 → feet 470
  const K = 220, D = 14; // underdamped: crit damping is ~29.7
  let x = 0, v = 0, raf = 0, dragging = false, startY = 0, grabbed = 0;

  function paint() {
    const s = Math.min(1.35, Math.max(0.55, 1 - x / 220));
    const lift = x < 0 ? x * 0.9 : 0;
    chick.style.transform = `translateY(${lift}px)`;
    legs.style.transform = `scale(1,${s})`; // origin is the feet, set in CSS
    body.style.transform = `translateY(${LEG_LEN * (1 - s)}px)`;
    const rot = -x * 0.12;
    wingL.style.transform = `translateY(${LEG_LEN * (1 - s)}px) rotate(${rot}deg)`;
    wingR.style.transform = `translateY(${LEG_LEN * (1 - s)}px) rotate(${-rot}deg)`;
    host.style.setProperty("--sx", 1 + (x - lift) / 600);
    host.style.setProperty("--so", Math.max(0.1, 0.45 + lift / 90));
  }

  function tick(now) {
    const dt = Math.min(0.032, (now - grabbed) / 1000);
    grabbed = now;
    v += (-K * x - D * v) * dt;
    x += v * dt;
    paint();
    if (Math.abs(x) < 0.3 && Math.abs(v) < 3) {
      raf = 0;
      host.classList.remove("drag");
      for (const g of [chick, legs, body, wingL, wingR]) g.style.transform = "";
      host.style.removeProperty("--sx");
      host.style.removeProperty("--so");
      return;
    }
    raf = requestAnimationFrame(tick);
  }

  host.addEventListener("pointerdown", (e) => {
    dragging = true;
    startY = e.clientY - x * (host.clientWidth / 512);
    v = 0;
    cancelAnimationFrame(raf);
    raf = 0;
    host.classList.add("drag");
    host.setPointerCapture(e.pointerId);
    paint();
  });

  host.addEventListener("pointermove", (e) => {
    if (!dragging) return;
    // clientY is CSS px; the SVG is drawn in a 512-unit viewBox.
    const scale = 512 / host.clientWidth;
    x = Math.max(-40, Math.min(150, (e.clientY - startY) * scale));
    paint();
  });

  const release = () => {
    if (!dragging) return;
    dragging = false;
    if (!raf) {
      grabbed = performance.now();
      raf = requestAnimationFrame(tick);
    }
  };
  host.addEventListener("pointerup", release);
  host.addEventListener("pointercancel", release);
})();
