export const Logo = (props: React.SVGProps<SVGSVGElement>) => (
  <svg
    xmlns="http://www.w3.org/2000/svg"
    width={1200}
    height={1200}
    role="graphics-symbol img"
    fill="currentColor"
    viewBox="0 0 900 900"
    {...props}
  >
    <title>Duckling Browser</title>
    <g fill="currentColor">
      <ellipse cx="470" cy="600" rx="280" ry="240" />
      <ellipse cx="410" cy="540" rx="200" ry="160" opacity="0.6" />
      <circle cx="380" cy="300" r="180" />
      <circle cx="340" cy="260" r="130" opacity="0.5" />
      <ellipse cx="320" cy="250" rx="40" ry="50" fill="#fff" />
      <circle cx="330" cy="245" r="22" fill="#1a1a2e" />
      <circle cx="338" cy="238" r="7" fill="#fff" />
      <circle cx="323" cy="250" r="3" fill="#fff" opacity="0.5" />
      <path
        d="M480 310 C530 300, 580 320, 590 340 C580 350, 520 348, 480 340 Z"
        opacity="0.85"
      />
      <path
        d="M480 340 C520 348, 580 350, 590 340 C570 365, 520 370, 480 355 Z"
        opacity="0.7"
      />
      <path
        d="M480 340 C530 344, 560 344, 590 340"
        stroke="#fff"
        strokeWidth="2"
        fill="none"
        opacity="0.3"
      />
      <ellipse
        cx="560"
        cy="560"
        rx="160"
        ry="110"
        opacity="0.8"
        transform="rotate(-15 560 560)"
      />
      <ellipse
        cx="540"
        cy="540"
        rx="110"
        ry="70"
        opacity="0.4"
        transform="rotate(-15 540 540)"
      />
      <path
        d="M700 520 C750 480, 780 510, 760 550 C740 560, 720 550, 700 540 Z"
        opacity="0.7"
      />
      <path
        d="M720 500 C760 460, 790 490, 780 530 C770 540, 750 530, 720 520 Z"
        opacity="0.8"
      />
      <path d="M340 130 C350 100, 370 80, 380 90 C390 80, 400 100, 395 130" />
      <ellipse
        cx="270"
        cy="330"
        rx="30"
        ry="20"
        fill="#FF7043"
        opacity="0.15"
      />
    </g>
  </svg>
);
