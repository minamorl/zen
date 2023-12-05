/*
  Warnings:

  - You are about to drop the column `email` on the `User` table. All the data in the column will be lost.
  - You are about to drop the column `password` on the `User` table. All the data in the column will be lost.

*/
-- DropIndex
DROP INDEX "User_email_key";

-- AlterTable
ALTER TABLE "User" DROP COLUMN "email",
DROP COLUMN "password";

-- CreateTable
CREATE TABLE "UserKey" (
    "id" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "hashedPassword" TEXT NOT NULL,

    CONSTRAINT "UserKey_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "UserSession" (
    "id" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "activeExpires" INTEGER NOT NULL,
    "idleExpires" INTEGER NOT NULL,

    CONSTRAINT "UserSession_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "UserKey_userId_key" ON "UserKey"("userId");

-- CreateIndex
CREATE UNIQUE INDEX "UserSession_userId_key" ON "UserSession"("userId");

-- AddForeignKey
ALTER TABLE "UserKey" ADD CONSTRAINT "UserKey_userId_fkey" FOREIGN KEY ("userId") REFERENCES "User"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "UserSession" ADD CONSTRAINT "UserSession_userId_fkey" FOREIGN KEY ("userId") REFERENCES "User"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
