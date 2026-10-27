import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { ChevronDown } from "lucide-react";

/** A text field with a list of known values: every value is one click away, and
 * typing narrows the list without preventing an id the list does not know. */
export default function ComboInput({
  id,
  value,
  onChange,
  choices,
  placeholder,
  mono = false,
  label,
}: {
  id: string;
  value: string;
  onChange: (value: string) => void;
  choices: string[];
  placeholder?: string;
  mono?: boolean;
  /** An accessible name when no `<label for>` points at the field. */
  label?: string;
}) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const [anchor, setAnchor] = useState<{
    left: number;
    top: number;
    width: number;
  } | null>(null);
  const wrapper = useRef<HTMLDivElement>(null);
  const field = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLUListElement>(null);
  const typed = value.trim().toLowerCase();
  // An exact or empty value shows everything; anything else narrows the list.
  const options =
    !typed || choices.some((choice) => choice.toLowerCase() === typed)
      ? choices
      : choices.filter((choice) => choice.toLowerCase().includes(typed));
  useEffect(() => {
    if (!open) return;
    // The list is fixed to the viewport so scrolling panels cannot clip it; any
    // scroll or resize outside the list itself closes it instead of leaving it
    // behind.
    const place = () => {
      const rect = field.current?.getBoundingClientRect();
      if (rect)
        setAnchor({ left: rect.left, top: rect.bottom + 4, width: rect.width });
    };
    place();
    const close = (event: Event) => {
      if (!wrapper.current?.contains(event.target as Node)) setOpen(false);
    };
    const scrolled = (event: Event) => {
      if (!list.current?.contains(event.target as Node)) setOpen(false);
    };
    window.addEventListener("pointerdown", close, true);
    window.addEventListener("scroll", scrolled, true);
    window.addEventListener("resize", place);
    return () => {
      window.removeEventListener("pointerdown", close, true);
      window.removeEventListener("scroll", scrolled, true);
      window.removeEventListener("resize", place);
    };
  }, [open]);
  useEffect(() => {
    if (open && active >= 0)
      list.current?.children[active]?.scrollIntoView({ block: "nearest" });
  }, [open, active]);
  const choose = (choice: string) => {
    onChange(choice);
    setOpen(false);
    setActive(-1);
  };
  const keyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setOpen(true);
      setActive((index) => Math.min(options.length - 1, index + 1));
    } else if (event.key === "ArrowUp" && open) {
      event.preventDefault();
      setActive((index) => Math.max(0, index - 1));
    } else if (event.key === "Enter" && open && options[active] !== undefined) {
      event.preventDefault();
      choose(options[active]);
    } else if (event.key === "Escape" && open) {
      event.preventDefault();
      event.stopPropagation();
      setOpen(false);
    }
  };
  const listId = `${id}-options`;
  return (
    <div className={`combo${open ? " is-open" : ""}`} ref={wrapper}>
      <input
        ref={field}
        id={id}
        className={mono ? "ins-mono" : undefined}
        role="combobox"
        aria-label={label}
        aria-expanded={open}
        aria-controls={listId}
        aria-autocomplete="list"
        aria-activedescendant={
          open && active >= 0 ? `${listId}-${active}` : undefined
        }
        value={value}
        placeholder={placeholder}
        autoComplete="off"
        spellCheck={false}
        onChange={(event) => {
          onChange(event.target.value);
          setOpen(true);
          setActive(-1);
        }}
        onClick={() => setOpen(true)}
        onKeyDown={keyDown}
      />
      <button
        type="button"
        className="combo-toggle"
        aria-label={open ? "Hide options" : "Show options"}
        tabIndex={-1}
        onPointerDown={(event) => event.preventDefault()}
        onClick={() => setOpen((state) => !state)}
      >
        <ChevronDown size={14} />
      </button>
      {open && anchor && options.length > 0 && (
        <ul
          id={listId}
          role="listbox"
          className="combo-list"
          ref={list}
          style={anchor}
        >
          {options.map((choice, index) => (
            <li
              key={choice}
              id={`${listId}-${index}`}
              role="option"
              aria-selected={choice === value.trim()}
              className={`combo-option${index === active ? " is-active" : ""}${mono ? " ins-mono" : ""}`}
              onPointerDown={(event) => event.preventDefault()}
              onMouseEnter={() => setActive(index)}
              onClick={(event) => {
                // Inside a wrapping <label>, a plain click would be forwarded
                // to the input and reopen the list.
                event.preventDefault();
                choose(choice);
              }}
            >
              {choice}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
