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
  // 中枢标题呼吸微光（静态标题的轻量样式，非切换动画）
  'central-title-glow': {
  '0%, 100%': { textShadow: '0 0 10px rgba(34,211,238,.55)' },
  '50%': { textShadow: '0 0 18px rgba(34,211,238,.95), 0 0 34px rgba(167,139,250,.5)' },
  },
  // 中枢视图入场（基础淡入，非高级转换效果）
  'central-view-in': {
  '0%': { opacity: '0', transform: 'scale(.985) translateY(6px)' },
  '100%': { opacity: '1', transform: 'scale(1) translateY(0)' },
  },
  },
  animation: {
  'central-title-glow': 'central-title-glow 2s ease-in-out 1',
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
