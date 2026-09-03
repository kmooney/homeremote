import { useCallback, useEffect, useRef, type ReactNode } from "react";

/** How long to hold before the first repeat, so a tap stays a single press. */
const DELAY_BEFORE_REPEAT = 400;

/**
 * Gap between repeats once they start.
 *
 * Paced to what the devices can actually absorb: an Apple TV button press is
 * two round trips, so repeating faster than this only builds a queue that
 * plays out after the finger lifts.
 */
const REPEAT_INTERVAL = 180;

/**
 * Repeats after this many, to close a long distance without a long wait.
 * The receiver moves in half steps, so plain repeat is slower than it looks.
 */
const ACCELERATE_AFTER = 8;
const FAST_INTERVAL = 110;

interface Props {
  onPress: () => void;
  /** Held buttons stop repeating when disabled mid-press. */
  disabled?: boolean;
  className?: string;
  "aria-label"?: string;
  "aria-pressed"?: boolean;
  "aria-current"?: boolean;
  children: ReactNode;
}

/**
 * A button that keeps firing while it is held down.
 *
 * Uses pointer events so it behaves the same under touch and mouse, and
 * captures the pointer so sliding a thumb off the button still stops the
 * repeat rather than leaving it running.
 */
export function HoldButton({ onPress, disabled, children, ...rest }: Props) {
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const count = useRef(0);
  // Kept in a ref so the running timer always calls the current handler.
  const handler = useRef(onPress);
  handler.current = onPress;

  const stop = useCallback(() => {
    if (timer.current) {
      clearTimeout(timer.current);
      timer.current = null;
    }
    count.current = 0;
  }, []);

  // Never leave a timer running behind a unmounted or disabled button.
  useEffect(() => stop, [stop]);
  useEffect(() => {
    if (disabled) stop();
  }, [disabled, stop]);

  const schedule = useCallback(
    (delay: number) => {
      timer.current = setTimeout(() => {
        handler.current();
        count.current += 1;
        schedule(count.current >= ACCELERATE_AFTER ? FAST_INTERVAL : REPEAT_INTERVAL);
      }, delay);
    },
    [],
  );

  const start = useCallback(
    (event: React.PointerEvent<HTMLButtonElement>) => {
      if (disabled) return;
      // Capture so we still get pointerup after the thumb slides away.
      event.currentTarget.setPointerCapture?.(event.pointerId);
      handler.current();
      count.current = 0;
      schedule(DELAY_BEFORE_REPEAT);
    },
    [disabled, schedule],
  );

  return (
    <button
      {...rest}
      disabled={disabled}
      onPointerDown={start}
      onPointerUp={stop}
      onPointerCancel={stop}
      // A long press on iOS otherwise pops the text-selection callout.
      onContextMenu={(e) => e.preventDefault()}
    >
      {children}
    </button>
  );
}
