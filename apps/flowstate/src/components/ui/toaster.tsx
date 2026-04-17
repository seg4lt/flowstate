import { useToast } from "@/hooks/use-toast";

export function Toaster() {
  const { toasts } = useToast();

  return (
    <div className="pointer-events-none fixed bottom-4 right-4 z-[100] flex max-w-xs flex-col gap-1">
      {toasts.map((toast) => (
        <div
          key={toast.id}
          className="pointer-events-auto rounded-md border border-border bg-background/95 px-3 py-2 text-sm shadow-md backdrop-blur-sm"
          data-state="open"
        >
          {toast.title && (
            <div className="font-semibold">{toast.title}</div>
          )}
          {toast.description && (
            <div className="opacity-90">{toast.description}</div>
          )}
        </div>
      ))}
    </div>
  );
}
