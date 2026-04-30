import { useEffect, useRef } from 'react';

export function TerminalPlayer({ src }: { src: string }) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!ref.current) return;
    const el = ref.current;
    let dispose: (() => void) | undefined;

    Promise.all([
      import('asciinema-player'),
      // @ts-expect-error — CSS module loaded for side-effects
      import('asciinema-player/dist/bundle/asciinema-player.css'),
    ]).then(([{ create }]) => {
      const player = create(src, el, {
        cols: 100,
        rows: 18,
        autoPlay: true,
        loop: true,
        preload: true,
        controls: false,
        terminalFontFamily: '"Cascadia Code", "Fira Code", "JetBrains Mono", monospace',
        terminalFontSize: '13px',
        theme: 'monokai',
      });
      dispose = () => player.dispose();
    });

    return () => dispose?.();
  }, [src]);

  return <div ref={ref} />;
}
