import { useEffect, useState, type RefObject } from "react";
import { ChevronDown } from "lucide-react";
import { Button } from "../ui/button";
import { isScrollContainerNearBottom } from "../../chat-scroll";

interface Props {
  scrollRef: RefObject<HTMLDivElement | null>;
  rowCount: number;
  onClick: () => void;
}

export function ScrollToBottomPill({ scrollRef, rowCount, onClick }: Props) {
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;

    const update = () => {
      setVisible(!isScrollContainerNearBottom(el));
    };

    update();
    el.addEventListener("scroll", update, { passive: true });
    return () => {
      el.removeEventListener("scroll", update);
    };
  }, [scrollRef, rowCount]);

  if (!visible) return null;

  return (
    <Button
      type="button"
      size="sm"
      variant="secondary"
      onClick={onClick}
      className="absolute bottom-4 right-6 z-10 shadow-lg gap-1 rounded-full"
    >
      <ChevronDown className="h-3.5 w-3.5" />
      Scroll to bottom
    </Button>
  );
}
