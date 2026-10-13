export function Placeholder({ title, text }: { title: string; text: string }) {
  return (
    <div className="empty">
      <h2>{title}</h2>
      <p style={{ maxWidth: 480 }}>{text}</p>
    </div>
  );
}
