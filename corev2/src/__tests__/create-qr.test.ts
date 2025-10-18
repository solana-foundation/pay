import { createQR, createQROptions, createStyledQRCode, createQRDataURL } from '../create-qr';

describe('createQR', () => {
    const sampleURL = 'solana:9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM?amount=1';

    describe('createQROptions', () => {
        it('should create default options', () => {
            const options = createQROptions(sampleURL);
            
            expect(options.width).toBe(512);
            expect(options.margin).toBe(2);
            expect(options.color?.dark).toBe('black');
            expect(options.color?.light).toBe('white');
            expect(options.errorCorrectionLevel).toBe('Q');
            expect(options.dotStyle).toBe('rounded');
            expect(options.cornerStyle).toBe('extra-rounded');
        });

        it('should create options with custom size and colors', () => {
            const options = createQROptions(sampleURL, 256, '#f0f0f0', '#333333');
            
            expect(options.width).toBe(256);
            expect(options.color?.dark).toBe('#333333');
            expect(options.color?.light).toBe('#f0f0f0');
        });
    });

    describe('createQR', () => {
        it('should create SVG QR code with default options', async () => {
            const svg = await createQR(sampleURL);
            
            expect(typeof svg).toBe('string');
            expect(svg.startsWith('<svg')).toBe(true);
            expect(svg.endsWith('</svg>')).toBe(true);
            expect(svg).toContain('width="512"');
            expect(svg).toContain('height="512"');
        });

        it('should create SVG QR code with custom size', async () => {
            const svg = await createQR(sampleURL, 256);
            
            expect(svg).toContain('width="256"');
            expect(svg).toContain('height="256"');
        });

        it('should create SVG QR code with custom colors', async () => {
            const svg = await createQR(sampleURL, 512, '#ffffff', '#000000');
            
            expect(svg).toContain('fill="#000000"');
            expect(svg).toContain('fill="#ffffff"');
        });

        it('should handle URL objects', async () => {
            const url = new URL(sampleURL);
            const svg = await createQR(url);
            
            expect(typeof svg).toBe('string');
            expect(svg.startsWith('<svg')).toBe(true);
        });
    });

    describe('createStyledQRCode', () => {
        it('should create styled QR code with square dots', async () => {
            const options = createQROptions(sampleURL);
            options.dotStyle = 'square';
            
            const svg = await createStyledQRCode(sampleURL, options);
            
            expect(svg).toContain('<rect');
            expect(svg).not.toContain('<circle');
        });

        it('should create styled QR code with dot circles', async () => {
            const options = createQROptions(sampleURL);
            options.dotStyle = 'dots';
            
            const svg = await createStyledQRCode(sampleURL, options);
            
            expect(svg).toContain('<circle');
        });

        it('should create styled QR code with rounded dots', async () => {
            const options = createQROptions(sampleURL);
            options.dotStyle = 'rounded';
            
            const svg = await createStyledQRCode(sampleURL, options);
            
            expect(svg).toContain('<rect');
            expect(svg).toContain('rx=');
        });

        it('should handle different corner styles', async () => {
            const cornerStyles = ['square', 'rounded', 'extra-rounded', 'full-rounded', 'maximum-rounded'];
            
            for (const cornerStyle of cornerStyles) {
                const options = createQROptions(sampleURL);
                options.cornerStyle = cornerStyle as any;
                
                const svg = await createStyledQRCode(sampleURL, options);
                
                expect(typeof svg).toBe('string');
                expect(svg.startsWith('<svg')).toBe(true);
            }
        });

        it('should include logo when provided', async () => {
            const options = createQROptions(sampleURL);
            options.logo = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==';
            options.logoSize = 64;
            options.logoBackgroundColor = 'white';
            options.logoMargin = 8;
            
            const svg = await createStyledQRCode(sampleURL, options);
            
            expect(svg).toContain('<image');
            expect(svg).toContain('width="64"');
            expect(svg).toContain('height="64"');
            expect(svg).toContain('fill="white"');
        });
    });

    describe('createQRDataURL', () => {
        it('should create data URL from SVG', async () => {
            const options = createQROptions(sampleURL);
            const dataURL = await createQRDataURL(sampleURL, options);
            
            expect(dataURL.startsWith('data:image/svg+xml;base64,')).toBe(true);
        });

        it('should create valid base64 encoded SVG', async () => {
            const options = createQROptions(sampleURL);
            const dataURL = await createQRDataURL(sampleURL, options);
            
            const base64Part = dataURL.replace('data:image/svg+xml;base64,', '');
            const decoded = Buffer.from(base64Part, 'base64').toString();
            
            expect(decoded.startsWith('<svg')).toBe(true);
            expect(decoded.endsWith('</svg>')).toBe(true);
        });
    });

    describe('Edge cases', () => {
        it('should handle very small sizes', async () => {
            const svg = await createQR(sampleURL, 50);
            
            expect(svg).toContain('width="50"');
            expect(svg).toContain('height="50"');
        });

        it('should handle very large sizes', async () => {
            const svg = await createQR(sampleURL, 2048);
            
            expect(svg).toContain('width="2048"');
            expect(svg).toContain('height="2048"');
        });

        it('should handle complex URLs', async () => {
            const complexURL = 'solana:9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM?amount=1.5&spl-token=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&reference=Fbri2ABzTqzq6KyGxtbE1VLe2vMU9GiGzMzUTsjJNyLk&label=Coffee%20Shop&message=Enjoy%20your%20coffee&memo=Order%20123';
            
            const svg = await createQR(complexURL);
            
            expect(typeof svg).toBe('string');
            expect(svg.startsWith('<svg')).toBe(true);
        });

        it('should handle undefined options gracefully', async () => {
            const options = createQROptions(sampleURL);
            options.color = undefined;
            
            const svg = await createStyledQRCode(sampleURL, options);
            
            expect(svg).toContain('fill="black"');
            expect(svg).toContain('fill="white"');
        });
    });
});