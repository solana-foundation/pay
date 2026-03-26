import { createQROptions } from '@solana/pay';
import QRCodeStyling from '@solana/qr-code-styling';
import React, { FC, useEffect, useMemo, useRef, useState } from 'react';
import { usePayment } from '../../hooks/usePayment';
import css from './QRCode.module.css';

export const QRCode: FC = () => {
    const [size, setSize] = useState(() =>
        typeof window === 'undefined' ? 400 : Math.min(window.screen.availWidth - 48, 400)
    );
    useEffect(() => {
        const listener = () => setSize(Math.min(window.screen.availWidth - 48, 400));

        window.addEventListener('resize', listener);
        return () => window.removeEventListener('resize', listener);
    }, []);

    const { url } = usePayment();
    const options = useMemo(() => createQROptions(url, size, 'transparent', '#2a2a2a'), [url, size]);

    const qr = useMemo(() => new QRCodeStyling(), []);
    useEffect(() => qr.update(options), [qr, options]);

    const ref = useRef<HTMLDivElement>(null);
    useEffect(() => {
        if (ref.current) {
            qr.append(ref.current);
        }
    }, [ref, qr]);

    return <div ref={ref} className={css.root} />;
};
