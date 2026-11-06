/** @type {import('tailwindcss').Config} */
export default {
  darkMode: ["class"],
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
  	extend: {
  		colors: {
  			border: 'hsl(var(--border))',
  			input: 'hsl(var(--input))',
  			ring: 'hsl(var(--ring))',
  			background: 'hsl(var(--background))',
  			foreground: 'hsl(var(--foreground))',
  			primary: {
  				DEFAULT: 'hsl(var(--primary))',
  				foreground: 'hsl(var(--primary-foreground))'
  			},
  			secondary: {
  				DEFAULT: 'hsl(var(--secondary))',
  				foreground: 'hsl(var(--secondary-foreground))'
  			},
  			muted: {
  				DEFAULT: 'hsl(var(--muted))',
  				foreground: 'hsl(var(--muted-foreground))'
  			},
  			accent: {
  				DEFAULT: 'hsl(var(--accent))',
  				foreground: 'hsl(var(--accent-foreground))'
  			},
  			destructive: {
  				DEFAULT: 'hsl(var(--destructive))',
  				foreground: 'hsl(var(--destructive-foreground))'
  			},
  			popover: {
  				DEFAULT: 'hsl(var(--popover))',
  				foreground: 'hsl(var(--popover-foreground))'
  			},
  			card: {
  				DEFAULT: 'hsl(var(--card))',
  				foreground: 'hsl(var(--card-foreground))'
  			},
  			sidebar: {
  				DEFAULT: 'hsl(var(--sidebar-background))',
  				foreground: 'hsl(var(--sidebar-foreground))',
  				primary: 'hsl(var(--sidebar-primary))',
  				'primary-foreground': 'hsl(var(--sidebar-primary-foreground))',
  				accent: 'hsl(var(--sidebar-accent))',
  				'accent-foreground': 'hsl(var(--sidebar-accent-foreground))',
  				border: 'hsl(var(--sidebar-border))',
  				ring: 'hsl(var(--sidebar-ring))'
  			}
  		},
  		borderRadius: {
  			lg: 'var(--radius)',
  			md: 'calc(var(--radius) - 2px)',
  			sm: 'calc(var(--radius) - 4px)'
  		},
  keyframes: {
  // Central mode transition (design draft 4.1): enter = full-screen sci-fi overlay (900ms), exit = quick collapse (280ms)
  'central-overlay-in': {
  '0%': { opacity: '0' },
  '14%': { opacity: '1' },
  '72%': { opacity: '1' },
  '100%': { opacity: '0' },
  },
  'central-overlay-out': {
  '0%': { opacity: '1' },
  '100%': { opacity: '0' },
  },
  'central-ring': {
  '0%': { transform: 'translate(-50%, -50%) scale(.2)', opacity: '0' },
  '18%': { opacity: '.95' },
  '100%': { transform: 'translate(-50%, -50%) scale(2.6)', opacity: '0' },
  },
  'central-sweep': {
  '0%': { top: '-2%' },
  '100%': { top: '102%' },
  },
  'central-grid': {
  '0%': { opacity: '0' },
  '30%': { opacity: '1' },
  '75%': { opacity: '1' },
  '100%': { opacity: '0' },
  },
  'central-title': {
  '0%': { opacity: '0', transform: 'scale(.86)', letterSpacing: '1.2em' },
  '35%': { opacity: '1' },
  '80%': { opacity: '1' },
  '100%': { opacity: '0', transform: 'scale(1)', letterSpacing: '.5em' },
  },
  'central-view-in': {
  '0%': { opacity: '0', transform: 'scale(.985) translateY(6px)' },
  '100%': { opacity: '1', transform: 'scale(1) translateY(0)' },
  },
  },
  animation: {
  'central-overlay-in': 'central-overlay-in 900ms ease-in-out forwards',
  'central-overlay-out': 'central-overlay-out 280ms ease-out forwards',
  'central-ring': 'central-ring 820ms cubic-bezier(.2,.7,.3,1) forwards',
  'central-ring-late': 'central-ring 820ms cubic-bezier(.2,.7,.3,1) 140ms forwards',
  'central-sweep': 'central-sweep 760ms cubic-bezier(.4,.1,.3,1) forwards',
  'central-grid': 'central-grid 900ms ease-in-out forwards',
  'central-title': 'central-title 900ms cubic-bezier(.2,.7,.3,1) forwards',
  'central-view-in': 'central-view-in 450ms cubic-bezier(.2,.7,.3,1) both',
  },
  	}
  },
  plugins: [require("tailwindcss-animate")],
  future: {
    // hover: 只在支持悬停的设备上生效（@media (hover: hover)）。触屏（手机远程访问）
    // 上 iOS Safari 会把首次点击当作悬停以呈现 hover 样式变化（如会话行的删除按钮
    // 由透明变为可见），导致要点两次才切换会话。
    hoverOnlyWhenSupported: true,
  },
}
