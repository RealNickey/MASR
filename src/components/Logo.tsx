import React from "react";
import logo from "../assets/logo.png";

interface LogoProps {
  size?: "sm" | "lg";
  className?: string;
}

export const Logo: React.FC<LogoProps> = ({ size = "lg", className = "" }) => {
  const isSm = size === "sm";
  const imgSize = isSm ? "h-7 w-7" : "h-10 w-10";
  const textSize = isSm ? "text-xl" : "text-3xl";
  const gap = isSm ? "gap-2" : "gap-3";

  return (
    <div className={`flex items-center ${gap} select-none ${className}`}>
      <img src={logo} alt="Logo" className={`${imgSize} object-contain`} />
      <span
        className={`${textSize} font-bold text-charcoal font-cooper tracking-wide`}
      >
        Thegai
      </span>
    </div>
  );
};
