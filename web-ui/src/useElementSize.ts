import { useEffect, useRef, useState } from 'react';

/// The size of the element a graph is drawn into, kept current as it changes.
///
/// react-force-graph does not measure its container. Given no width/height it
/// falls back to the window's inner size AT MOUNT and never looks again, so the
/// canvas is frozen at whatever the window happened to be when the view was
/// created: resizing the window moves the panels around a graph that stays the
/// size it was born. In 3D the same stale numbers reach the camera's aspect and
/// zoomToFit, which is how a 346-unit graph ended up framed from 2,965 units
/// away (bead neurostrata-0o1).
///
/// Returns 0x0 until the first measurement, which callers must treat as "do not
/// draw yet" rather than passing on to a renderer.
export const useElementSize = () => {
  const ref = useRef<HTMLDivElement | null>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });

  useEffect(() => {
    const element = ref.current;
    if (!element) return;

    const measure = () => {
      const { width, height } = element.getBoundingClientRect();
      // Rounded because a fractional canvas size costs a resample every frame,
      // and only set when it actually changed: this feeds a prop, and a new
      // object every observation would re-render the graph continuously.
      const next = { width: Math.round(width), height: Math.round(height) };
      setSize(current =>
        current.width === next.width && current.height === next.height ? current : next
      );
    };

    measure();

    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  return { ref, ...size };
};
