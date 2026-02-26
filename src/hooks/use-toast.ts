import * as React from "react";

type ToastProps = {
  id: string;
  title?: string;
  description?: string;
  duration?: number;
};

type Toast = ToastProps;

type ToastActionType =
  | { type: "ADD_TOAST"; toast: Toast }
  | { type: "REMOVE_TOAST"; id: string };

const toastReducer = (state: Toast[], action: ToastActionType): Toast[] => {
  switch (action.type) {
    case "ADD_TOAST":
      return [...state, action.toast];
    case "REMOVE_TOAST":
      return state.filter((toast) => toast.id !== action.id);
    default:
      return state;
  }
};

const listeners: Array<(state: Toast[]) => void> = [];
let memoryState: Toast[] = [];

function dispatch(action: ToastActionType) {
  memoryState = toastReducer(memoryState, action);
  listeners.forEach((listener) => listener(memoryState));
}

let toastCount = 0;

function genId() {
  toastCount = (toastCount + 1) % Number.MAX_SAFE_INTEGER;
  return toastCount.toString();
}

export function toast(props: Omit<ToastProps, "id">) {
  const id = genId();
  const duration = props.duration ?? 3000;

  dispatch({
    type: "ADD_TOAST",
    toast: {
      ...props,
      id,
      duration,
    },
  });

  setTimeout(() => {
    dispatch({ type: "REMOVE_TOAST", id });
  }, duration);

  return {
    id,
    dismiss: () => dispatch({ type: "REMOVE_TOAST", id }),
  };
}

export function useToast() {
  const [state, setState] = React.useState<Toast[]>(memoryState);

  React.useEffect(() => {
    listeners.push(setState);
    return () => {
      const index = listeners.indexOf(setState);
      if (index > -1) {
        listeners.splice(index, 1);
      }
    };
  }, []);

  return {
    toasts: state,
    toast,
    dismiss: (id: string) => dispatch({ type: "REMOVE_TOAST", id }),
  };
}
