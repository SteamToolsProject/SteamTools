import type { ComponentChildren, JSX } from "preact";
import { h } from "preact";

export function Icon({ path }: { path: string }) {
  return (
    <svg className="stt-nav-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d={path} />
    </svg>
  );
}

export function Button({ children, danger = false, onClick, disabled = false }: {
  children: ComponentChildren;
  danger?: boolean;
  onClick?: JSX.MouseEventHandler<HTMLButtonElement>;
  disabled?: boolean;
}) {
  return <button type="button" className={`stt-button${danger ? " stt-button-danger" : ""}`} onClick={onClick} disabled={disabled}>{children}</button>;
}

export function Toggle({ checked, onChange, label, disabled = false }: { checked: boolean; onChange: () => void; label: string; disabled?: boolean }) {
  return (
    <button type="button" role="switch" aria-checked={checked} aria-label={label} aria-disabled={disabled || undefined} disabled={disabled} className={`stt-toggle${checked ? " stt-toggle-on" : ""}`} onClick={onChange}>
      <span className="stt-toggle-knob" />
    </button>
  );
}

export function Segmented({ values, current, labels, label = "选项", onChange }: {
  values: string[];
  current: string;
  labels?: Record<string, string>;
  label?: string;
  onChange: (value: string) => void;
}) {
  return (
    <div className="stt-segmented" role="group" aria-label={label}>
      {values.map((value) => <button type="button" key={value} className={`stt-segment${value === current ? " stt-segment-active" : ""}`} aria-pressed={value === current} onClick={() => onChange(value)}>{labels?.[value] ?? value}</button>)}
    </div>
  );
}

export function FormRow({ title, detail, mono = false, control }: {
  title: ComponentChildren;
  detail?: string;
  mono?: boolean;
  control?: ComponentChildren;
}) {
  return (
    <div className="stt-row">
      <div className="stt-row-main">
        <div className={`stt-row-title${mono ? " stt-row-title-mono" : ""}`} title={typeof title === "string" ? title : undefined}>{title}</div>
        {detail ? <div className="stt-row-detail" title={detail}>{detail}</div> : null}
      </div>
      {control ? <div className="stt-row-control">{control}</div> : null}
    </div>
  );
}

export function Lead({ title, description, channel }: { title: string; description: string; channel?: string }) {
  return (
    <div className="stt-lead">
      <div className="stt-kicker">{title}</div>
      <div className="stt-description">{description}</div>
      {channel !== undefined ? <div className="stt-channel"><span className="stt-channel-dot" /><span>{channel ? `通道已连接 · ${channel}` : "等待通道"}</span></div> : null}
    </div>
  );
}

export function Notice({ message }: { message: string }) {
  const error = message.startsWith("失败");
  return <div className={`stt-note${error ? " stt-note-error" : ""}`} role={error ? "alert" : "status"}>{message}</div>;
}

export function EmptyState({ title, detail }: { title: string; detail: string }) {
  return <div className="stt-empty-state"><div className="stt-empty-mark" aria-hidden="true">—</div><div className="stt-empty-title">{title}</div><div className="stt-empty-detail">{detail}</div></div>;
}

export function LoadingState() {
  return <div className="stt-loading" role="status">正在读取配置...</div>;
}
