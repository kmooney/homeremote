import { useEffect, useRef, useState } from "react";

interface Props {
  /** What the field on the device holds. */
  text: string;
  /** Replace the device's field; resolves once the device has it. */
  onChange: (text: string) => Promise<void>;
}

/**
 * Types into the text field the Apple TV has selected.
 *
 * Shown only while a field is selected there. Each change sends the whole
 * text rather than the keystroke, so a change that is skipped or arrives late
 * cannot leave the two out of step.
 */
export function TextEntry({ text, onChange }: Props) {
  const [value, setValue] = useState(text);
  const input = useRef<HTMLInputElement>(null);

  // Follow the device, except while someone is typing here: the device's
  // echo of an earlier keystroke would otherwise undo the ones since.
  useEffect(() => {
    if (document.activeElement !== input.current) setValue(text);
  }, [text]);

  // One update in flight at a time, and only the newest waiting behind it.
  // Typing outpaces the device, and every intermediate state is moot.
  const sending = useRef(false);
  const waiting = useRef<string | null>(null);
  const handler = useRef(onChange);
  handler.current = onChange;

  const send = async (next: string) => {
    waiting.current = next;
    if (sending.current) return;
    sending.current = true;
    while (waiting.current !== null) {
      const latest = waiting.current;
      waiting.current = null;
      await handler.current(latest);
    }
    sending.current = false;
  };

  return (
    <input
      ref={input}
      className="text-entry"
      type="text"
      placeholder="Type to the Apple TV"
      aria-label="Text for the Apple TV"
      autoComplete="off"
      autoCorrect="off"
      autoCapitalize="off"
      spellCheck={false}
      enterKeyHint="done"
      // Typing should start without reaching for the mouse.
      autoFocus
      value={value}
      onChange={(e) => {
        setValue(e.target.value);
        void send(e.target.value);
      }}
      onKeyDown={(e) => {
        // Hand the keyboard back to the remote, so the arrows reach the
        // Apple TV again and can move on to the results.
        if (e.key === "Enter" || e.key === "Escape") e.currentTarget.blur();
      }}
    />
  );
}
